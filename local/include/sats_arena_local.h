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
    uint64_t revision, budget_sats;
    uint64_t available_sats[2], reserved_sats[2];
    char invoices[2][128];
    char error[256];
} ArenaLocalView;
/* Phase: 0 loading, 1 funding, 2 ready, 3 playing, 4 settled, 5 cancelled, 6 disputed, 7 error.
 * Actions: 1 new, 2 simulate P1 payment, 3 simulate P2 payment, 4 start,
 * 5 cancel before start, 6 P1 wins, 7 P2 wins, 8 no contest (game-event notifications in the FAKE-ONLY host).
 * No HTTP, NWC configuration, or real payment backend exists in this host.
 */
ArenaLocalHandle* arena_local_create(const char* folder);
bool arena_local_snapshot(const ArenaLocalHandle*, ArenaLocalView* out);
bool arena_local_action(const ArenaLocalHandle*, uint32_t action);
void arena_local_destroy(ArenaLocalHandle*);
#ifdef __cplusplus
}
#endif
