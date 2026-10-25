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
    uint64_t paid_sats[2], remaining_sats[2], earned_sats[2], pending_sats[2];
    char invoices[2][2048];
    char error[256];
    char addresses[2][128];
    uint64_t batch_remaining_ms[2];
    char payment_log[4096];
    uint32_t verification_state[2];
    uint64_t min_sendable_msat[2], max_sendable_msat[2];
    char verification_message[2][256];
    uint64_t verification_checked_ms[2];
} ArenaLocalView;
/* ABI 4. Two fresh entry invoices every match. No reusable account balance.
 * Phase: 0 loading, 1 awaiting entry, 2 paid, 3 playing, 4 paid out,
 * 6 payouts pending, 7 error.
 * Actions: 1 next match, 2/3 simulate entry payment P1/P2, 4 start,
 * 5 cancel, 6/7 winner P1/P2, 8 no contest.
 * credit(): cumulative finalized attributed damage received by P1/P2, centi-percent.
 * Duplicate counters are safe. New match resets counters to zero.
 * Fake incoming/outgoing wallet only. No real Lightning connection.
 */
ArenaLocalHandle* arena_local_create(const char* folder);
bool arena_local_snapshot(const ArenaLocalHandle*, ArenaLocalView* out);
bool arena_local_action(const ArenaLocalHandle*, uint32_t action);
bool arena_local_credit(const ArenaLocalHandle*, uint64_t p1_damage_centi, uint64_t p2_damage_centi);
bool arena_local_addresses(const ArenaLocalHandle*, const char* p1, const char* p2);
/* Verify only this player (0/1), preserving the other receiving address/check. */
bool arena_local_verify_address(const ArenaLocalHandle*, uint32_t player, const char* address);
void arena_local_destroy(ArenaLocalHandle*);
#ifdef __cplusplus
}
#endif
