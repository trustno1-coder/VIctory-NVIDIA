// BCPack's lane decoding (victory-formats bcpack.rs `lanes`), in C: the same
// schedule, two passes a round (every lane's step, then the bytes in lane
// order). The Rust version stays the reference and the fallback
// (`VICTORY_BCPACK_C=0`); the tests compare the two.
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#define LOW (1u << 24)
#define ROUND_BYTES 8
typedef struct { uint8_t width, phase, longf, reset, model, need, has_latch, latch, has_out, out, stalled; uint32_t x, left; } Lane;

static size_t bucket_of(const uint32_t *start, const uint8_t *bucket, uint32_t slot) {
    size_t b = bucket[slot >> 8];
    while (start[b + 1] <= slot) b++;
    return b;
}

/* freq [128][9], start [128][10], bucket [128][256], recip [128][9]: the tables
   as bcpack.rs keeps them. 0: done; -1: the stream ends early or is corrupt. */
int victory_bcpack_lanes(const uint32_t *freq /*[128][9]*/, const uint32_t *start /*[128][10]*/,
                   const uint8_t *bucket /*[128][256]*/, const uint64_t *recip /*[128][9]*/,
                   const uint8_t *widths, size_t nl, size_t per, size_t blocks,
                   const uint8_t *src, size_t srclen, uint8_t *records) {
    Lane lanes[16]; memset(lanes, 0, sizeof lanes);
    for (size_t j = 0; j < nl; j++) { lanes[j].width = widths[j]; lanes[j].need = 1; }
    size_t stride = nl * per, pos = 0, at = 0, sp = 0; uint32_t done = 0, full = (1u << nl) - 1; int idle = 0;
    while (pos < blocks * per) {
        int taken = 0, moved = 0;
        // pass 1: the lanes' steps, independent of each other
        for (size_t j = 0; j < nl; j++) {
            Lane *l = &lanes[j];
            if (!l->stalled) {
                if (l->phase != 3) {
                    if (l->has_latch) {
                        uint8_t b = l->latch; l->has_latch = 0; l->need = 1;
                        if (l->phase == 0) { uint8_t g = l->width - (b >> 6); l->model = g * 16 + (b & 15); l->reset = (b & 0x10) != 0; l->longf = (b & 0x20) != 0; l->left = 0; l->phase = 1; }
                        else if (l->phase == 1 && l->longf) { l->left = b; l->phase = 2; }
                        else if (l->phase == 1) { l->left = (uint32_t)b + 1; l->phase = 3; }
                        else { l->left = (((uint32_t)b << 8) | l->left) + 257; l->phase = 3; }
                    }
                } else {
                    uint32_t x = l->x;
                    if (x < LOW && l->has_latch) { l->has_latch = 0; l->left--; x = (x << 8) | l->latch; }
                    l->need = l->left != 0 && !l->has_latch;
                    if (x > LOW) {
                        const uint32_t *f = freq + l->model * 9, *st = start + l->model * 10;
                        uint32_t slot = x & 0xffff;
                        size_t b = bucket_of(st, bucket + l->model * 256, slot);
                        uint32_t sym, fr, rem;
                        if (b == 0) { sym = 0; fr = f[0] - 1; rem = slot - 1; }
                        else { uint32_t off = slot - st[b]; uint32_t q = (uint32_t)(((uint64_t)off * recip[l->model * 9 + b]) >> 32); sym = (1u << (b - 1)) + q; fr = f[b]; rem = off - q * f[b]; }
                        l->x = (x >> 16) * fr + rem; l->has_out = 1; l->out = (uint8_t)sym;
                    } else if (l->left != 0) { l->x = x; }
                    else { l->phase = 0; l->need = !l->has_latch; if (l->reset) l->x = 0; }
                }
            }
        }
        // pass 2: bytes, stalls and outputs, in lane order
        for (size_t j = 0; j < nl; j++) {
            Lane *l = &lanes[j]; uint32_t bit = 1u << j; int before = taken;
            int waiting = l->has_out && (done & bit);
            if (l->need && !waiting && taken < ROUND_BYTES) { if (sp < srclen) { l->latch = src[sp++]; l->has_latch = 1; taken++; } }
            l->stalled = (l->need && before >= ROUND_BYTES) || waiting;
            if (!l->stalled && l->has_out) { records[at + j * per] = l->out; l->has_out = 0; done |= bit; moved = 1; }
        }
        if (done == full) { pos++; at += (pos % per == 0) ? stride - per + 1 : 1; done = 0; moved = 1; }
        idle = (taken > 0 || moved) ? 0 : idle + 1;
        if (idle > 3) return -1;
    }
    return 0;
}
