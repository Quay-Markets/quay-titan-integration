use super::*;
use crate::trading_venue::venue_creation::ParsedInstruction;
use crate::trading_venue::AddressLookupTableTrait;

/// Build a fake `Clock` sysvar account body. Fills the full 40-byte
/// layout — the decoder must read only `slot` (0..8) and
/// `unix_timestamp` (32..40), ignoring `epoch_start_timestamp`,
/// `epoch`, and `leader_schedule_epoch`. We poison those middle fields
/// with a non-zero pattern to catch any offset drift.
fn fake_clock(slot: u64, unix_ts: i64) -> Vec<u8> {
    let mut data = vec![0u8; CLOCK_MIN_LEN];
    data[0..8].copy_from_slice(&slot.to_le_bytes());
    // Poison middle fields — decoder must NOT pick these up.
    data[8..16].copy_from_slice(&0xDEAD_BEEF_DEAD_BEEFu64.to_le_bytes());
    data[16..24].copy_from_slice(&0xFEED_FACE_FEED_FACEu64.to_le_bytes());
    data[24..32].copy_from_slice(&0xBAAD_F00D_BAAD_F00Du64.to_le_bytes());
    data[32..40].copy_from_slice(&unix_ts.to_le_bytes());
    data
}

#[test]
fn decode_clock_reads_slot_and_unix_ts() {
    let data = fake_clock(123_456_789, 1_700_000_000);
    let (slot, ts) = decode_clock(&data).expect("decode should succeed");
    assert_eq!(slot, 123_456_789);
    assert_eq!(ts, 1_700_000_000);
}

#[test]
fn decode_clock_rejects_short_buffers() {
    // 39 bytes — one short of the unix_timestamp tail.
    let data = vec![0u8; CLOCK_MIN_LEN - 1];
    assert!(decode_clock(&data).is_none());
}

#[test]
fn decode_clock_accepts_oversized_buffers() {
    // Real sysvar accounts are >= 40 bytes; future Solana releases
    // could grow the struct. The decoder should ignore trailing bytes.
    let mut data = fake_clock(7, 11);
    data.extend_from_slice(&[0xAB; 32]);
    let (slot, ts) = decode_clock(&data).expect("decode should succeed");
    assert_eq!(slot, 7);
    assert_eq!(ts, 11);
}

#[test]
fn decode_clock_handles_negative_unix_ts() {
    // Pre-1970 timestamps are nonsensical for mainnet but the field is
    // `i64` — the decoder must preserve the sign rather than coerce.
    let data = fake_clock(0, -1);
    let (_, ts) = decode_clock(&data).expect("decode should succeed");
    assert_eq!(ts, -1);
}

/// Build a `QuayVenue` with every halt / freeze byte cleared —
/// `halts_clear()` returns true, and (once state blobs are populated)
/// `initialized()` returns true.
fn all_active_venue() -> QuayVenue {
    QuayVenue {
        program_id: Pubkey::new_unique(),
        strategy_key: Pubkey::new_unique(),
        // Non-empty so `has_all_state()` passes — content doesn't
        // matter for the halt-gate tests since they exercise
        // `halts_clear()` directly.
        strategy_data: vec![0u8; 1],
        mm_key: Pubkey::new_unique(),
        mm_data: vec![0u8; 1],
        quotes_key: Pubkey::new_unique(),
        quotes_data: vec![0u8; 1],
        global_config_key: Pubkey::new_unique(),
        global_config_data: vec![0u8; 1],
        base_mint: Pubkey::new_unique(),
        quote_mint: Pubkey::new_unique(),
        vault_base_key: Pubkey::new_unique(),
        vault_quote_key: Pubkey::new_unique(),
        tokens: Vec::new(),
        routing_flags: ROUTE_TITAN,
        cfg_swap_halted: 0,
        cfg_protocol_halted: 0,
        strategy_frozen: 0,
        strategy_frozen_admin: 0,
        mm_frozen: 0,
        mm_frozen_admin: 0,
        mm_halted_admin: 0,
        price_probe_base: 0,
        price_probe_quote: 0,
        current_slot: 0,
        current_unix_sec: 0,
    }
}

#[test]
fn initialized_true_when_state_loaded_and_halts_clear() {
    assert!(all_active_venue().initialized());
}

/// One row of the halt-gate property table — see Jupiter sibling.
type HaltCase = (&'static str, fn(&mut QuayVenue));

/// Property: setting any one halt / freeze byte to non-zero must flip
/// `initialized()` to false. Mirrors the Jupiter sibling test —
/// adding a new flag forces a code change here.
#[test]
fn initialized_false_when_any_single_halt_set() {
    let cases: &[HaltCase] = &[
        ("cfg_swap_halted", |v| v.cfg_swap_halted = 1),
        ("cfg_protocol_halted", |v| v.cfg_protocol_halted = 1),
        ("strategy_frozen", |v| v.strategy_frozen = 1),
        ("strategy_frozen_admin", |v| v.strategy_frozen_admin = 1),
        ("mm_frozen", |v| v.mm_frozen = 1),
        ("mm_frozen_admin", |v| v.mm_frozen_admin = 1),
        ("mm_halted_admin", |v| v.mm_halted_admin = 1),
    ];
    for (name, set) in cases {
        let mut venue = all_active_venue();
        set(&mut venue);
        assert!(!venue.initialized(), "flag {name}=1 should fail initialized()");
    }
}

#[test]
fn initialized_false_when_state_missing() {
    // Pre-first-update behavior: state blobs are empty so the
    // `has_all_state()` arm of the gate fails even though the warmup
    // defaults make `halts_clear()` false too. We force halts to clear
    // here so the test exercises the state-populated branch in
    // isolation.
    let mut venue = all_active_venue();
    venue.mm_data.clear();
    assert!(!venue.initialized(), "empty mm_data should fail initialized()");
}

#[test]
fn initialized_false_when_not_titan_routed() {
    // The MM must opt a strategy into Titan via `routing_flags & ROUTE_TITAN`.
    let mut venue = all_active_venue();
    venue.routing_flags = 0;
    assert!(!venue.initialized(), "no routing bits set must not route");
    venue.routing_flags = 0x02; // ROUTE_JUPITER only — not Titan.
    assert!(
        !venue.initialized(),
        "another router's bit alone must not enable Titan"
    );
    venue.routing_flags = ROUTE_TITAN | 0x02;
    assert!(venue.initialized(), "ROUTE_TITAN set (with others) routes");
}

#[test]
fn required_pubkeys_include_clock_sysvar() {
    // Sanity check: the index used in `update_state` (slot 8) must
    // match the position of `SYSVAR_CLOCK_ID` in the required-keys
    // vector. If anyone reorders these the off-by-one bites loudly.
    let venue = all_active_venue();
    let keys = venue.get_required_pubkeys_for_update().unwrap();
    assert_eq!(keys.len(), 9);
    assert_eq!(keys[8], SYSVAR_CLOCK_ID);
}

#[test]
fn parse_pool_creations_reads_pair_from_init_strategy() {
    use quay_sdk::consts::DISC_INIT_STRATEGY;

    let strategy = Pubkey::new_unique();
    let base = Pubkey::new_unique();
    let quote = Pubkey::new_unique();
    let filler = Pubkey::new_unique();

    // init_strategy accounts: [strategy, mm, cfg, owner, system, base, quote].
    let init = ParsedInstruction {
        program_id: QUAY_PROGRAM_ID,
        accounts: vec![strategy, filler, filler, filler, filler, base, quote],
        data: vec![DISC_INIT_STRATEGY, 0, 0, 0, 0, 0, 0],
    };
    // Ignored: a non-Quay program, and a Quay non-`init_strategy` ix (swap).
    let foreign = ParsedInstruction {
        program_id: Pubkey::new_unique(),
        accounts: vec![strategy, filler, filler, filler, filler, base, quote],
        data: vec![DISC_INIT_STRATEGY],
    };
    let quay_swap = ParsedInstruction {
        program_id: QUAY_PROGRAM_ID,
        accounts: vec![strategy],
        data: vec![0x20], // DISC_SWAP
    };
    // Ignored: a malformed `init_strategy` missing the mint accounts.
    let short = ParsedInstruction {
        program_id: QUAY_PROGRAM_ID,
        accounts: vec![strategy, filler],
        data: vec![DISC_INIT_STRATEGY],
    };

    let pools = parse_pool_creations(&[foreign, init, quay_swap, short]);
    assert_eq!(pools.len(), 1, "only the well-formed Quay init_strategy yields a pool");
    assert_eq!(pools[0].pool, strategy);
    assert_eq!(pools[0].mints, vec![base, quote]);
}

#[tokio::test]
async fn lookup_table_keys_cover_stable_accounts_no_cache() {
    let v = all_active_venue();
    let keys = v.get_lookup_table_keys(None).await.unwrap();

    // Every stable venue account is present.
    for k in [
        v.program_id,
        v.global_config_key,
        v.strategy_key,
        v.mm_key,
        v.quotes_key,
        v.vault_base_key,
        v.vault_quote_key,
        v.base_mint,
        v.quote_mint,
    ] {
        assert!(keys.contains(&k), "ALT keys should include {k}");
    }
    // Token + system programs are included.
    assert!(keys.contains(&pda::SPL_TOKEN_PROGRAM_ID));
    assert!(keys.contains(&pda::TOKEN_2022_PROGRAM_ID));
    // Per-taker accounts are user-specific and must NOT be baked into the ALT.
    // (We have no taker here; assert the set is exactly the stable accounts.)
    assert_eq!(keys.len(), 13, "10 venue accounts + 3 program deps");
}
