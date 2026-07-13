# rfofs-client

C-ABI dynamic library for controlling an already-running `rfofs` JACK process
from an external program (e.g. Racket via `ffi/unsafe`). Talks to `rfofs`
over a POSIX shared-memory control plane (see `../src/shm.rs`) — no socket,
no polling loop needed by the caller.

Build: `cargo build --release -p rfofs-client` produces
`target/release/librfofs_client.so`.

## API

```c
void*    rfofs_connect(void);
void     rfofs_disconnect(void* handle);
int32_t  rfofs_add_fof(void* handle,
                        uint64_t id, uint64_t start_sample,
                        float f, float gliss, float phi, float amp,
                        float alpha, float beta, float fade_level, float fade_dur,
                        float azm, float elev, float distance);
int32_t  rfofs_kill(void* handle, uint64_t id, float fade_dur);
int32_t  rfofs_get_stats(void* handle, RfofsStats* out);
```

`rfofs_connect` returns null if no running `rfofs` is found (or its shared
segment doesn't match this build). `rfofs_add_fof`/`rfofs_kill` return 0 on
success, -1 for a null/invalid handle, -2 if the shared request/kill ring is
momentarily full (retry). `RfofsStats` is four `uint64_t` fields: `too_late`,
`too_early`, `slot_full`, `queue_size` — see `rfofs::queue::QueueStats` for
what each counts.

## Racket usage

```racket
#lang racket
(require ffi/unsafe ffi/unsafe/define)

(define-ffi-definer define-rfofs (ffi-lib "librfofs_client"))

(define-rfofs rfofs_connect (_fun -> _pointer))
(define-rfofs rfofs_disconnect (_fun _pointer -> _void))
(define-rfofs rfofs_add_fof
  (_fun _pointer _uint64 _uint64
        _float _float _float _float
        _float _float _float _float
        _float _float _float
        -> _int32))
(define-rfofs rfofs_kill (_fun _pointer _uint64 _float -> _int32))

(define handle (rfofs_connect))
(unless handle (error 'rfofs "no running rfofs found"))

;; Fire-and-forget FOF: id=0, ~0.1s from now at 44.1kHz, A440.
(rfofs_add_fof handle 0 4410 440.0 0.0 0.0 0.5 10.0 0.01 0.001 0.01 0.0 0.0 1.0)

(rfofs_disconnect handle)
```

## Manual verification

With `rfofs` running (`cargo run --release` from the workspace root):

```sh
cargo run --release -p rfofs-client --example shm_client_smoke
```

Connects, submits a fire-and-forget FOF and a tracked+killed FOF, prints live
stats, and disconnects. Run it without `rfofs` running first to confirm
`rfofs_connect` fails gracefully instead of crashing.
