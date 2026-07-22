/* C-ABI header for librfofs_client — see rfofs-client/src/lib.rs for the
 * canonical doc comments and rfofs-client/README.md for a usage example.
 *
 * Every function takes an opaque ClientHandle* obtained from rfofs_connect().
 * All functions are safe to call from any thread, but only one client
 * process is expected to be attached to a given rfofs instance at a time.
 * A null handle is accepted everywhere and treated as a no-op / error return
 * (see each function's return value below) rather than crashing.
 */

#ifndef RFOFS_CLIENT_H
#define RFOFS_CLIENT_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Clock mode constants — mirror rfofs::clock::RFOFS_CLOCK_* (src/clock.rs).
 * Numeric values are fixed for cross-process compatibility. */
#define RFOFS_CLOCK_JACK_FRAME_TIME 1u
#define RFOFS_CLOCK_JACK_TRANSPORT  2u

/* Opaque handle returned by rfofs_connect(); pass to every other call. */
typedef struct ClientHandle ClientHandle;

/* Snapshot of the queue stats living in the shared control block. */
typedef struct RfofsStats {
    uint64_t too_late;
    uint64_t too_early;
    uint64_t slot_full;
    uint64_t queue_size;
} RfofsStats;

/* Attempt to attach to an already-running rfofs's control plane.
 * Returns NULL if no running rfofs was found, or if the segment found
 * doesn't match this build's wire format (magic/version mismatch). */
ClientHandle* rfofs_connect(void);

/* Release a handle obtained from rfofs_connect(). handle must not be used
 * again after this call. A null handle is a no-op. */
void rfofs_disconnect(ClientHandle* handle);

/* The audio server's sample rate, in Hz. Returns 0.0 if handle is null. */
float rfofs_sample_rate(ClientHandle* handle);

/* The audio server's nominal buffer size, in frames. Individual process
 * callbacks may report fewer frames than this; it's the value to plan
 * around (e.g. for scheduling headroom). Returns 0 if handle is null. */
uint32_t rfofs_block_size(ClientHandle* handle);

/* The server's currently active clock mode — RFOFS_CLOCK_JACK_FRAME_TIME or
 * RFOFS_CLOCK_JACK_TRANSPORT. This is whatever the server was started with,
 * or a later value set by any client via rfofs_set_clock_mode() — it is not
 * fixed for the server's lifetime. Returns 0 if handle is null. */
uint32_t rfofs_clock_mode(ClientHandle* handle);

/* Switch which JACK time source drives the server's sample clock. Takes
 * effect on the server's next process block after the write is observed —
 * there is no explicit acknowledgment, poll rfofs_clock_mode() if you need
 * to confirm it landed. mode must be RFOFS_CLOCK_JACK_FRAME_TIME or
 * RFOFS_CLOCK_JACK_TRANSPORT.
 *
 * Returns 0 on success, -1 if handle is null, -2 if mode isn't one of the
 * known constants (the previous mode is left in place).
 *
 * Caution: switching to a source reporting a smaller sample count than the
 * one currently active (e.g. away from frame-time, which only grows, to a
 * transport that's stopped or was just relocated) can silently stall new
 * FOF admission until the new source's value grows back past where the
 * server's scheduler had already reached. Switching to a larger-valued
 * source is always safe. */
int32_t rfofs_set_clock_mode(ClientHandle* handle, uint32_t mode);

/* The engine's current absolute sample clock. Callers must submit
 * start_sample values at or beyond this (plus some future headroom to
 * absorb the bridging thread's poll latency) — start_sample is an absolute
 * sample count since the server started, not relative to the client's
 * connection time. Returns 0 if handle is null. */
uint64_t rfofs_current_sample(ClientHandle* handle);

/* Submit a new FOF onset. id == 0 is fire-and-forget; nonzero ids are
 * individually killable via rfofs_kill().
 *
 * Returns 0 on success, -1 if handle is null, -2 if the shared request ring
 * is full (the caller is submitting faster than rfofs can drain it — retry
 * later). */
int32_t rfofs_add_fof(ClientHandle* handle,
                       uint64_t id, uint64_t start_sample,
                       float f, float gliss, float phi, float amp,
                       float alpha, float beta, float fade_level, float fade_dur,
                       float azm, float elev, float distance);

/* Request an early fade-out on a tracked (nonzero-id) FOF. No-op on the
 * engine side if id doesn't match any currently active FOF.
 *
 * Returns 0 on success, -1 if handle is null, -2 if the shared kill ring is
 * full. */
int32_t rfofs_kill(ClientHandle* handle, uint64_t id, float fade_dur);

/* Whether the connected server was built with the `statistics` feature,
 * i.e. whether the counts read back by rfofs_get_stats() are actually being
 * tracked. When this returns false, rfofs_get_stats() still succeeds but
 * every field reads back as 0 regardless of real scheduling activity.
 * Returns false if handle is null. */
bool rfofs_stats_enabled(ClientHandle* handle);

/* Read a live snapshot of the queue stats into *out.
 *
 * Returns 0 on success, -1 if handle or out is null. */
int32_t rfofs_get_stats(ClientHandle* handle, RfofsStats* out);

#ifdef __cplusplus
}
#endif

#endif /* RFOFS_CLIENT_H */
