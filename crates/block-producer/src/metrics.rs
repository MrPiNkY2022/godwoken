use std::collections::HashMap;

use gw_common::{CKB_SUDT_SCRIPT_ARGS, H256};
use gw_otel::metric::{
    counter::Counter,
    encoding::text::Encode,
    family::Family,
    gauge::Gauge,
    prometheus_client,
    registry::{Registry, Unit},
    OnceCell,
};
use gw_store::{traits::chain_store::ChainStore, Store};
use gw_types::prelude::Unpack;
use serde::Deserialize;
use tracing::instrument;

pub const ENV_METRIC_MONITOR_CUSTODIAN_ENABLE: &str = "METRIC_MONITOR_CUSTODIAN_ENABLE";
pub const ENV_METRIC_MONITOR_CUSTODIAN_VEC_JSON: &str = "METRIC_MONITOR_CUSTODIAN_VEC_JSON";

pub fn bp() -> &'static BlockProducerMetrics {
    static METRICS: OnceCell<BlockProducerMetrics> = OnceCell::new();
    METRICS.get_or_init(|| {
        let metrics = BlockProducerMetrics::default();
        let mut registry = gw_otel::metric::global_registry();
        metrics.register(registry.sub_registry_with_prefix("block_producer"));
        metrics
    })
}

pub fn custodian() -> &'static CustodianMetrics {
    static METRICS: OnceCell<CustodianMetrics> = OnceCell::new();
    METRICS.get_or_init(|| {
        let mut metrics = CustodianMetrics::default();
        debug_assert!(!metrics.enabled);

        let maybe_enable = std::env::var(ENV_METRIC_MONITOR_CUSTODIAN_ENABLE);
        if matches!(maybe_enable.as_deref(), Ok("true")) {
            metrics.enabled = true;
            let mut registry = gw_otel::metric::global_registry();
            metrics.register(registry.sub_registry_with_prefix("custodian"));
        }

        metrics
    })
}

#[derive(Default)]
pub struct BlockProducerMetrics {
    pub resend: Counter,
    pub witness_size: Counter,
    pub tx_size: Counter,
    pub sync_buffer_len: Gauge,
    pub local_blocks: Gauge,
    pub submitted_blocks: Gauge,
}

impl BlockProducerMetrics {
    fn register(&self, registry: &mut Registry) {
        registry.register(
            "resend",
            "Number of times resending submission transactions",
            Box::new(self.resend.clone()),
        );
        registry.register_with_unit(
            "witness_size",
            "Block submission txs witness size",
            Unit::Bytes,
            Box::new(self.witness_size.clone()),
        );
        registry.register_with_unit(
            "tx_size",
            "Block submission txs size",
            Unit::Bytes,
            Box::new(self.tx_size.clone()),
        );
        registry.register(
            "sync_buffer_len",
            "Number of messages in the block sync receive buffer",
            Box::new(self.sync_buffer_len.clone()),
        );
        registry.register(
            "local_blocks",
            "Number of local blocks",
            Box::new(self.local_blocks.clone()),
        );
        registry.register(
            "submitted_blocks",
            "Number of submitted blocks",
            Box::new(self.submitted_blocks.clone()),
        );
    }
}

#[derive(Default)]
pub struct CustodianMetrics {
    enabled: bool,
    finalized_custodians: Family<CustodianLabel, Gauge>,
}

impl CustodianMetrics {
    fn register(&self, registry: &mut Registry) {
        registry.register(
            "finalized_custodians",
            "Finalized custodians",
            Box::new(self.finalized_custodians.clone()),
        );
    }

    pub fn finalized_custodians(&self, store: &Store) {
        if !self.enabled {
            return;
        }

        let get_local_finalized = { store.get_last_valid_tip_block_hash().ok() }
            .and_then(|bh| store.get_block_number(&bh).ok().flatten())
            .map(|bn| store.get_block_post_finalized_custodian_capacity(bn));

        let local = match get_local_finalized.flatten() {
            Some(local) => local.as_reader().unpack(),
            None => return,
        };

        let cal = |balance: &u128, decimal| balance.saturating_div(10u128.pow(decimal)) as u64;
        for custodian in Self::custodian_map().values() {
            if custodian.type_hash == H256::from(CKB_SUDT_SCRIPT_ARGS) {
                self.finalized_ckb(|g, d| g.set(cal(&(local.capacity as u128), d)));
            }
            if let Some((balance, _)) = local.sudt.get::<[u8; 32]>(&custodian.type_hash.into()) {
                self.finalized(custodian.type_hash, |g, d| g.set(cal(balance, d)));
            }
        }
    }

    pub fn finalized_ckb<F, O>(&self, f: F) -> Option<O>
    where
        F: Fn(&Gauge, u32) -> O,
    {
        if !self.enabled {
            return None;
        }

        self.finalized(CKB_SUDT_SCRIPT_ARGS.into(), f)
    }

    pub fn finalized<F, O>(&self, type_hash: H256, f: F) -> Option<O>
    where
        F: Fn(&Gauge, u32) -> O,
    {
        if !self.enabled {
            return None;
        }

        let map = Self::custodian_map();
        let custodian = map.get(&type_hash)?;

        let gauge = self.finalized_custodians.get_or_create(&CustodianLabel {
            symbol: &custodian.symbol,
        });

        Some(f(&gauge, custodian.decimal))
    }

    fn custodian_map() -> &'static HashMap<H256, Custodian> {
        static MAP: OnceCell<HashMap<H256, Custodian>> = OnceCell::new();
        MAP.get_or_init(|| Self::parse_map_from_env().unwrap_or_default())
    }

    #[instrument(skip_all, err(Debug))]
    fn parse_map_from_env() -> anyhow::Result<HashMap<H256, Custodian>> {
        #[derive(Deserialize, Debug)]
        struct JsonCustodian {
            pub symbol: String,
            pub type_hash: String,
            pub decimal: u32,
        }

        let json = std::env::var(ENV_METRIC_MONITOR_CUSTODIAN_VEC_JSON)?;
        tracing::info!("env metric monitor custodian vec json {}", json);
        let vec = serde_json::from_str::<Vec<JsonCustodian>>(&json)?;
        tracing::info!("parsed vec {:?}", vec);

        let to_custodian = vec.into_iter().map(|jc| -> Result<_, hex::FromHexError> {
            let mut buf = [0u8; 32];
            hex::decode_to_slice(&jc.type_hash, &mut buf)?;

            let c = Custodian {
                symbol: jc.symbol,
                type_hash: H256::from(buf),
                decimal: jc.decimal,
            };
            tracing::info!("monitor add {}", c.symbol);

            Ok((H256::from(buf), c))
        });

        Ok(to_custodian.collect::<Result<_, _>>()?)
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct Custodian {
    pub symbol: String,
    pub type_hash: H256,
    pub decimal: u32,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Encode)]
struct CustodianLabel {
    pub symbol: &'static str,
}
