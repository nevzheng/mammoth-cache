//! The admin API (`api/proto/mammoth/admin/v1/admin.proto`), served as HTTP/JSON.
//!
//! Responses use the proto3 JSON mapping: lowerCamelCase fields, `int64` as strings, durations as
//! `"1.5s"` and enums by name. The types here are written by hand for now; generating them from the
//! proto (prost and pbjson) is planned once a second admin method exists. Authentication (design
//! doc 0002) is not wired up yet.

use std::time::Duration;

use axum::{Json, extract::State};
use serde::Serialize;

use super::AppState;
use crate::store::Tier;

/// `mammoth.admin.v1.ServerStatus`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ServerStatus {
    name: &'static str,
    version: &'static str,
    uptime: String,
    hit_count: String,
    miss_count: String,
    tiers: Vec<TierStatus>,
}

/// `mammoth.admin.v1.TierStatus`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TierStatus {
    tier: &'static str,
    entry_count: String,
    size_bytes: String,
    capacity_bytes: String,
}

/// `GET /v1/serverStatus` (AIP-131, AIP-156).
pub(super) async fn get_server_status(State(state): State<AppState>) -> Json<ServerStatus> {
    let status = state.cache.status();
    Json(ServerStatus {
        name: "serverStatus",
        version: env!("CARGO_PKG_VERSION"),
        uptime: proto_duration(status.uptime),
        hit_count: status.hits.to_string(),
        miss_count: status.misses.to_string(),
        tiers: status
            .tiers
            .into_iter()
            .map(|(tier, usage)| TierStatus {
                tier: proto_tier(tier),
                entry_count: usage.entries.to_string(),
                size_bytes: usage.size_bytes.to_string(),
                capacity_bytes: usage.capacity_bytes.to_string(),
            })
            .collect(),
    })
}

/// A `google.protobuf.Duration` in its JSON form, e.g. `"3.000000001s"`.
fn proto_duration(d: Duration) -> String {
    match d.subsec_nanos() {
        0 => format!("{}s", d.as_secs()),
        nanos => format!("{}.{nanos:09}s", d.as_secs()),
    }
}

fn proto_tier(tier: Tier) -> &'static str {
    match tier {
        Tier::Memory => "TIER_MEMORY",
        Tier::Disk => "TIER_DISK",
        Tier::Bucket => "TIER_BUCKET",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_use_proto_json_form() {
        assert_eq!(proto_duration(Duration::from_secs(3)), "3s");
        assert_eq!(proto_duration(Duration::new(3, 1)), "3.000000001s");
    }
}
