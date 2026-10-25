#pragma once
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#else
#include <stdbool.h>
#endif
typedef struct ArenaLocalHandle ArenaLocalHandle;
typedef struct ArenaLocalView {
    uint32_t abi_version, phase, busy, simulated;
    uint64_t revision, entry_sats, sats_per_percent;
    uint64_t paid_sats[4], remaining_sats[4], earned_sats[4], pending_sats[4];
    char invoices[4][2048];
    char error[256];
    char addresses[4][128];
    uint64_t batch_remaining_ms[4];
    char payment_log[4096];
    uint32_t verification_state[4];
    uint64_t min_sendable_msat[4], max_sendable_msat[4];
    char verification_message[4][256];
    uint64_t verification_checked_ms[4];
    uint32_t teams, kinds[4], team_ids[4], characters[4], roster_configured;
    uint64_t last_credit_sequence;
    uint32_t sandbox;
} ArenaLocalView;
/* ABI 6. Fresh entry invoices for every participating human. No reusable account balance.
 * Phase: 0 loading, 1 awaiting entry, 2 paid, 3 playing, 4 paid out,
 * 6 payouts pending, 7 error.
 * Actions: 1 next match, 2/3 pay sandbox entry (or simulate in unit-test fake mode) P1/P2, 4 start,
 * 5 cancel, 6/7 winner P1/P2, 8 no contest, 9/10 fund P3/P4,
 * 11/12 winner P3/P4 (winner identifies whole side in teams).
 * credit(): cumulative finalized attributed damage received by P1/P2, centi-percent.
 * Duplicate counters are safe. New match resets counters to zero.
 * Fake/NWC incoming and outgoing wallets; live receiving-address verification.
 */
ArenaLocalHandle* arena_local_create(const char* folder);
/* Separate fake/live roots; live state is additionally bound to wallet identity. */
ArenaLocalHandle* arena_local_create_configured(const char* fake_folder, const char* live_folder);
bool arena_local_snapshot(const ArenaLocalHandle*, ArenaLocalView* out);
bool arena_local_action(const ArenaLocalHandle*, uint32_t action);
bool arena_local_credit(const ArenaLocalHandle*, uint64_t p1_damage_centi, uint64_t p2_damage_centi);
bool arena_local_addresses(const ArenaLocalHandle*, const char* p1, const char* p2);
/* Verify only this player (0..3), preserving the other receiving address/check. */
bool arena_local_verify_address(const ArenaLocalHandle*, uint32_t player, const char* address);
typedef struct ArenaLocalCredit {
    uint64_t sequence, damage_centi;
    uint32_t attacker, victim;
} ArenaLocalCredit;
/* kinds: human=0,CPU=1,closed=3; teams: red0,blue1,green2 or255 in FFA/closed.
 * characters: CSS icon IDs; arrays contain four values. Changed funded rosters
 * return entries before creating fresh invoices. */
bool arena_local_roster(const ArenaLocalHandle*, uint32_t teams, const uint32_t* kinds,
                        const uint32_t* team_ids, const uint32_t* characters);
/* Ordered finalized credits. Retry-safe sequences start at1 each match. */
bool arena_local_credits(const ArenaLocalHandle*, const ArenaLocalCredit*, uint32_t count);
void arena_local_destroy(ArenaLocalHandle*);
#ifdef __cplusplus
}
#endif
