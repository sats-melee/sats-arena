#pragma once
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#else
#include <stdbool.h>
#endif
typedef struct ArenaLocalHandle ArenaLocalHandle;
typedef struct ArenaLocalView {
    uint32_t abi_version, phase, busy, padding;
    uint64_t revision, entry_sats, sats_per_percent;
    uint64_t paid_sats[2], remaining_sats[2], earned_sats[2], pending_sats[2];
    char invoices[2][128];
    char error[256];
} ArenaLocalView;
/* ABI 2. Two fresh entry invoices every match. No reusable account balance.
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
void arena_local_destroy(ArenaLocalHandle*);
#ifdef __cplusplus
}
#endif
