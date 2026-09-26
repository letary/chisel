//! Shared by the integration tests.

/// The SDK's vocabulary manifest (`fixtures/vocab.json`, a copy of LeCodes' `sdk/src/chisel.ts`
/// held equal by its `scripts/chisel-parity.ts`) — what every test bundle is fed, like a real
/// LeCodes compile.
pub fn sdk_vocab() -> Option<serde_json::Value> {
    Some(serde_json::from_str(include_str!("../../../../fixtures/vocab.json")).expect("fixtures/vocab.json is JSON"))
}
