#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, IntoVal, Vec};

/// Errors that can be returned by the Oracle Consumer contract.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Error {
    /// The price data returned by the oracle is older than the configured
    /// `MaxPriceAge` threshold and must not be used.
    StalePriceFeed = 1,
    /// The price data returned by the oracle is older than the configured
    /// `MaxStaleness` threshold and must not be used.
    OraclePriceStale = 2,
}

const DEFAULT_TWAP_WINDOW_SECONDS: u64 = 300;
const DEFAULT_MAX_PRICE_AGE_SECONDS: u64 = 600;
const DEFAULT_MAX_OBSERVATIONS: u32 = 24;
const DEFAULT_MAX_STALENESS_SECONDS: u64 = 300;

/// Standardized data structure for price, timestamp, and asset.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceData {
    pub asset: Address,
    pub price: i128,
    pub timestamp: u64,
}

#[contracttype]
pub enum DataKey {
    OracleAddress,
    PriceRecord(Address),
    PriceHistory(Address),
    Admin,
    DefaultTwapWindow,
    MaxPriceAge,
    MaxObservations,
    MaxStaleness,
}

#[contract]
pub struct OracleConsumer;

#[allow(deprecated)]
#[contractimpl]
impl OracleConsumer {
    /// Initializes the consumer with an admin and the initial oracle source.
    pub fn initialize(env: Env, admin: Address, oracle: Address) {
        if env.storage().instance().has(&DataKey::OracleAddress) {
            panic!("already initialized");
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::OracleAddress, &oracle);
        env.storage()
            .instance()
            .set(&DataKey::DefaultTwapWindow, &DEFAULT_TWAP_WINDOW_SECONDS);
        env.storage()
            .instance()
            .set(&DataKey::MaxPriceAge, &DEFAULT_MAX_PRICE_AGE_SECONDS);
        env.storage()
            .instance()
            .set(&DataKey::MaxObservations, &DEFAULT_MAX_OBSERVATIONS);
        env.storage()
            .instance()
            .set(&DataKey::MaxStaleness, &DEFAULT_MAX_STALENESS_SECONDS);
    }

    /// Pulls the latest price for a given asset from the configured external oracle.
    /// This updates the local storage with fresh data, appends it to the local
    /// observation history used for TWAP calculation, and returns it.
    ///
    /// Returns [`Error::OraclePriceStale`] if the oracle-reported timestamp is
    /// older than the configured `MaxStaleness` threshold.
    pub fn update_price(env: Env, asset: Address) -> Result<PriceData, Error> {
        let oracle: Address = env
            .storage()
            .instance()
            .get(&DataKey::OracleAddress)
            .expect("oracle not set");

        let price_info: PriceData = env.invoke_contract(
            &oracle,
            &symbol_short!("get_price"),
            (asset.clone(),).into_val(&env),
        );

        assert!(
            price_info.asset == asset,
            "oracle returned mismatched asset"
        );
        assert!(price_info.price > 0, "oracle returned non-positive price");

        let max_staleness: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MaxStaleness)
            .unwrap_or(DEFAULT_MAX_STALENESS_SECONDS);
        Self::assert_not_stale(&env, price_info.timestamp, max_staleness)?;

        env.storage()
            .instance()
            .set(&DataKey::PriceRecord(asset.clone()), &price_info);
        Self::store_observation(&env, asset.clone(), price_info.clone());

        // Topic: event name only; asset + price in data.
        env.events().publish(
            (symbol_short!("oracle"), symbol_short!("price_upd")),
            (asset, price_info.price),
        );

        Ok(price_info)
    }

    /// Retrieves the most recent locally stored spot price for an asset.
    /// Includes a staleness check based on the provided `max_age_seconds`.
    pub fn get_latest_price(
        env: Env,
        asset: Address,
        max_age_seconds: u64,
    ) -> Result<i128, Error> {
        let price_info = Self::get_price_record(&env, asset);
        Self::assert_not_stale(&env, price_info.timestamp, max_age_seconds)?;
        Ok(price_info.price)
    }

    /// Returns the TWAP over the requested lookback window.
    ///
    /// The calculation uses piecewise-constant pricing between observations and
    /// requires history that reaches at or before the start of the requested
    /// window to avoid a single fresh update dominating the average.
    pub fn get_twap_price(
        env: Env,
        asset: Address,
        lookback_seconds: u64,
        max_age_seconds: u64,
    ) -> Result<i128, Error> {
        assert!(lookback_seconds > 0, "lookback window must be positive");

        let current_time = env.ledger().timestamp();
        let latest = Self::get_price_record(&env, asset.clone());
        Self::assert_not_stale(&env, latest.timestamp, max_age_seconds)?;

        let window_start = current_time.saturating_sub(lookback_seconds);
        let history = Self::get_price_history(&env, asset);

        let mut covered = false;
        let mut weighted_sum: i128 = 0;

        for i in 0..history.len() {
            let observation = history.get(i).unwrap();
            let next_timestamp = if i + 1 < history.len() {
                history.get(i + 1).unwrap().timestamp
            } else {
                current_time
            };

            if observation.timestamp <= window_start {
                covered = true;
            }

            let interval_start = if observation.timestamp > window_start {
                observation.timestamp
            } else {
                window_start
            };
            let interval_end = if next_timestamp < current_time {
                next_timestamp
            } else {
                current_time
            };

            if interval_end > interval_start {
                weighted_sum = weighted_sum
                    .checked_add(
                        observation
                            .price
                            .checked_mul((interval_end - interval_start) as i128)
                            .expect("twap multiplication overflow"),
                    )
                    .expect("twap accumulation overflow");
            }
        }

        assert!(
            covered,
            "insufficient price history for requested twap window"
        );

        Ok(weighted_sum / lookback_seconds as i128)
    }

    /// Default consumer-facing price read.
    ///
    /// This returns the configured TWAP instead of the latest spot price so
    /// downstream contracts can consume a manipulation-resistant value.
    pub fn get_price(env: Env, asset: Address) -> Result<i128, Error> {
        let lookback: u64 = env
            .storage()
            .instance()
            .get(&DataKey::DefaultTwapWindow)
            .unwrap_or(DEFAULT_TWAP_WINDOW_SECONDS);
        let max_staleness: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MaxStaleness)
            .unwrap_or(DEFAULT_MAX_STALENESS_SECONDS);

        Self::get_twap_price(env, asset, lookback, max_staleness)
    }

    /// Reconfigures the oracle source address. Restricted to the administrator.
    pub fn set_oracle(env: Env, new_oracle: Address) {
        let admin = Self::get_admin(&env);
        admin.require_auth();

        env.storage()
            .instance()
            .set(&DataKey::OracleAddress, &new_oracle);
    }

    /// Updates the default TWAP lookback window. Restricted to the administrator.
    pub fn set_twap_window(env: Env, lookback_seconds: u64) {
        let admin = Self::get_admin(&env);
        admin.require_auth();

        assert!(lookback_seconds > 0, "lookback window must be positive");
        env.storage()
            .instance()
            .set(&DataKey::DefaultTwapWindow, &lookback_seconds);
    }

    /// Updates the maximum acceptable age for the latest observation used by TWAP.
    pub fn set_max_price_age(env: Env, max_age_seconds: u64) {
        let admin = Self::get_admin(&env);
        admin.require_auth();

        assert!(max_age_seconds > 0, "max price age must be positive");
        env.storage()
            .instance()
            .set(&DataKey::MaxPriceAge, &max_age_seconds);
    }

    /// Updates the maximum acceptable staleness (in seconds) for oracle price
    /// feed timestamps. Restricted to the administrator.
    pub fn set_max_staleness(env: Env, max_staleness_seconds: u64) {
        let admin = Self::get_admin(&env);
        admin.require_auth();

        assert!(
            max_staleness_seconds > 0,
            "max staleness must be positive"
        );
        env.storage()
            .instance()
            .set(&DataKey::MaxStaleness, &max_staleness_seconds);
    }

    /// Returns the currently configured maximum staleness threshold in seconds.
    pub fn get_max_staleness(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::MaxStaleness)
            .unwrap_or(DEFAULT_MAX_STALENESS_SECONDS)
    }

    /// Reverts with [`Error::OraclePriceStale`] when the supplied price
    /// timestamp is older than `max_staleness_seconds` relative to the current
    /// ledger timestamp.
    fn assert_not_stale(
        env: &Env,
        price_timestamp: u64,
        max_staleness_seconds: u64,
    ) -> Result<(), Error> {
        let now = env.ledger().timestamp();
        if now.saturating_sub(price_timestamp) > max_staleness_seconds {
            return Err(Error::OraclePriceStale);
        }
        Ok(())
    }

    fn get_admin(env: &Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("admin not set")
    }

    fn get_price_record(env: &Env, asset: Address) -> PriceData {
        env.storage()
            .instance()
            .get(&DataKey::PriceRecord(asset))
            .expect("no price record for asset")
    }

    fn get_price_history(env: &Env, asset: Address) -> Vec<PriceData> {
        env.storage()
            .instance()
            .get(&DataKey::PriceHistory(asset))
            .unwrap_or(Vec::new(env))
    }

    fn store_observation(env: &Env, asset: Address, price_info: PriceData) {
        let mut history = Self::get_price_history(env, asset.clone());
        history.push_back(price_info);

        let max_observations: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MaxObservations)
            .unwrap_or(DEFAULT_MAX_OBSERVATIONS);

        while history.len() > max_observations {
            history.pop_front();
        }

        env.storage()
            .instance()
            .set(&DataKey::PriceHistory(asset), &history);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};

    fn setup(env: &Env) -> (OracleConsumerClient, Address) {
        env.mock_all_auths();
        let contract_id = env.register_contract(None, OracleConsumer);
        let client = OracleConsumerClient::new(env, &contract_id);
        let admin = Address::generate(env);
        let oracle = Address::generate(env);
        client.initialize(&admin, &oracle);
        (client, admin)
    }

    #[test]
    fn test_set_max_staleness_requires_admin() {
        let env = Env::default();
        let (client, _admin) = setup(&env);

        client.set_max_staleness(&120);
        assert_eq!(client.get_max_staleness(), 120);
    }

    #[test]
    fn test_set_max_staleness_rejects_zero() {
        let env = Env::default();
        let (client, _admin) = setup(&env);

        let result = client.try_set_max_staleness(&0);
        assert!(result.is_err());
    }

    #[test]
    fn test_assert_not_stale_rejects_old_timestamp() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);

        // Timestamp 400s in the past exceeds the 300s threshold.
        let result = OracleConsumer::assert_not_stale(&env, 700, 300);
        assert_eq!(result, Err(Error::OraclePriceStale));
    }

    #[test]
    fn test_assert_not_stale_accepts_fresh_timestamp() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);

        // Timestamp 100s in the past is within the 300s threshold.
        let result = OracleConsumer::assert_not_stale(&env, 900, 300);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn test_assert_not_stale_boundary_is_inclusive() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);

        // Exactly at the threshold should be accepted.
        let result = OracleConsumer::assert_not_stale(&env, 700, 300);
        assert_eq!(result, Ok(()));
    }
}
