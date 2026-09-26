/* H.264 motion compensation inner loops (8.4.2.2), written so the C
 * compiler vectorises them with SSE2. MayOS builds its kernel without
 * SIMD; this file alone is compiled with it (its kernel threads save the
 * SSE registers on every switch). Only integers and pointers cross the
 * boundary, so the calling convention matches either way.
 *
 * `s` points at the block's integer-sample origin, with samples valid
 * from 2 left/above to 3 right/below the w x h block. Output stride `os`.
 */
#include <stdint.h>

static inline uint8_t clip(int v) { return v < 0 ? 0 : v > 255 ? 255 : (uint8_t)v; }

#define TAP(a, b, c, d, e, f) ((a) - 5 * ((b) + (e)) + 20 * ((c) + (d)) + (f))

/* Horizontal half sample b (rounded) at row r. */
static inline void hrow(const uint8_t *r, int w, int16_t *o) {
    for (int i = 0; i < w; i++) {
        int t = TAP(r[i - 2], r[i - 1], r[i], r[i + 1], r[i + 2], r[i + 3]);
        o[i] = clip((t + 16) >> 5);
    }
}

/* Vertical half sample h (rounded) for the row starting at r. */
static inline void vrow(const uint8_t *r, int st, int w, int16_t *o) {
    for (int i = 0; i < w; i++) {
        int t = TAP(r[i - 2 * st], r[i - st], r[i], r[i + st], r[i + 2 * st], r[i + 3 * st]);
        o[i] = clip((t + 16) >> 5);
    }
}

void mayos_h264_luma(const uint8_t *s, int st, int fx, int fy, int w, int h, uint8_t *out, int os) {
    int16_t a[16], b[16];
    if (fx == 0 && fy == 0) {
        for (int j = 0; j < h; j++)
            for (int i = 0; i < w; i++) out[j * os + i] = s[j * st + i];
        return;
    }
    if (fy == 0) {
        for (int j = 0; j < h; j++) {
            const uint8_t *r = s + j * st;
            hrow(r, w, a);
            uint8_t *o = out + j * os;
            if (fx == 2)
                for (int i = 0; i < w; i++) o[i] = (uint8_t)a[i];
            else {
                const uint8_t *g = r + (fx == 3);
                for (int i = 0; i < w; i++) o[i] = (uint8_t)((g[i] + a[i] + 1) >> 1);
            }
        }
        return;
    }
    if (fx == 0) {
        for (int j = 0; j < h; j++) {
            const uint8_t *r = s + j * st;
            vrow(r, st, w, a);
            uint8_t *o = out + j * os;
            if (fy == 2)
                for (int i = 0; i < w; i++) o[i] = (uint8_t)a[i];
            else {
                const uint8_t *g = r + (fy == 3) * st;
                for (int i = 0; i < w; i++) o[i] = (uint8_t)((g[i] + a[i] + 1) >> 1);
            }
        }
        return;
    }
    if (fx == 2 || fy == 2) {
        /* Centre sample j from unrounded horizontal taps of rows -2..h+3. */
        int32_t raw[21][16];
        for (int r = 0; r < h + 5; r++) {
            const uint8_t *l = s + (r - 2) * st;
            for (int i = 0; i < w; i++) raw[r][i] = TAP(l[i - 2], l[i - 1], l[i], l[i + 1], l[i + 2], l[i + 3]);
        }
        for (int j = 0; j < h; j++) {
            uint8_t *o = out + j * os;
            if (fx != 2) vrow(s + j * st + (fx == 3), st, w, b);
            const int32_t *r0 = raw[j], *r1 = raw[j + 1], *r2 = raw[j + 2], *r3 = raw[j + 3], *r4 = raw[j + 4], *r5 = raw[j + 5];
            for (int i = 0; i < w; i++) {
                int jv = clip((TAP(r0[i], r1[i], r2[i], r3[i], r4[i], r5[i]) + 512) >> 10);
                int v;
                if (fx == 2 && fy == 2) v = jv;
                else if (fx == 2) v = (clip(((fy == 1 ? r2[i] : r3[i]) + 16) >> 5) + jv + 1) >> 1;
                else v = (b[i] + jv + 1) >> 1;
                o[i] = (uint8_t)v;
            }
        }
        return;
    }
    /* Diagonal quarter positions: average of horizontal half sample of row
     * j or j+1 and vertical half sample of column i or i+1. */
    for (int j = 0; j < h; j++) {
        hrow(s + (j + (fy == 3)) * st, w, a);
        vrow(s + j * st + (fx == 3), st, w, b);
        uint8_t *o = out + j * os;
        for (int i = 0; i < w; i++) o[i] = (uint8_t)((a[i] + b[i] + 1) >> 1);
    }
}

/* Chroma (eighth sample, bilinear); samples valid up to 1 right/below. */
void mayos_h264_chroma(const uint8_t *s, int st, int fx, int fy, int w, int h, uint8_t *out, int os) {
    const uint16_t wa = (8 - fx) * (8 - fy), wb = fx * (8 - fy), wc = (8 - fx) * fy, wd = fx * fy;
    for (int j = 0; j < h; j++) {
        const uint8_t *r0 = s + j * st, *r1 = r0 + st;
        uint8_t *o = out + j * os;
        for (int i = 0; i < w; i++)
            o[i] = (uint8_t)((uint16_t)(wa * r0[i] + wb * r0[i + 1] + wc * r1[i] + wd * r1[i + 1] + 32) >> 6);
    }
}

/* ---- YUV 4:2:0 -> 0xAARRGGBB with bilinear luma scaling ----
 * Tables (from Rust) give, per destination row, the two source luma row
 * offsets, the vertical weight (0..256) and the chroma row offset; per
 * destination column, the two source luma columns, the horizontal weight
 * and the chroma column. Scratch: `tmp` (>= xe entries), `yb`, `ub`, `vb`
 * (>= dw entries). coef = {cr_r, cb_g, cr_g, cb_b, ymul, yoff}. */
void mayos_yuv_scale(const uint8_t *y, const uint8_t *u, const uint8_t *v,
                     const int32_t *ry0, const int32_t *ry1, const int32_t *rfy, const int32_t *rc, int dh,
                     const int32_t *x0, const int32_t *x1, const int32_t *xf, const int32_t *xc, int dw,
                     int xb, int xe, const int32_t *coef, uint32_t *dst, int ds,
                     uint16_t *tmp, int16_t *yb, int16_t *ub, int16_t *vb) {
    const int cr_r = coef[0], cb_g = coef[1], cr_g = coef[2], cb_b = coef[3], ymul = coef[4], yoff = coef[5];
    for (int j = 0; j < dh; j++) {
        const uint8_t *r0 = y + ry0[j], *r1 = y + ry1[j];
        const uint16_t fy = (uint16_t)rfy[j], gy = (uint16_t)(256 - fy);
        /* Vertical blend (8.8 fixed point, fits 16 bits). */
        if (fy == 0)
            for (int i = xb; i < xe; i++) tmp[i] = (uint16_t)(r0[i] << 8);
        else
            for (int i = xb; i < xe; i++) tmp[i] = (uint16_t)(r0[i] * gy + r1[i] * fy);
        const uint8_t *cu = u + rc[j], *cv = v + rc[j];
        /* Horizontal: gather into contiguous rows (direct when 1:1). */
        if (dw == xe - xb) {
            for (int i = 0; i < dw; i++) yb[i] = (int16_t)((tmp[xb + i] + 128) >> 8);
            for (int i = 0; i < dw; i++) {
                ub[i] = (int16_t)(cu[xc[i]] - 128);
                vb[i] = (int16_t)(cv[xc[i]] - 128);
            }
        } else
        for (int i = 0; i < dw; i++) {
            uint32_t a = tmp[x0[i]], b = tmp[x1[i]], f = (uint32_t)xf[i];
            yb[i] = (int16_t)((a * (256 - f) + b * f + 32768) >> 16);
            ub[i] = (int16_t)(cu[xc[i]] - 128);
            vb[i] = (int16_t)(cv[xc[i]] - 128);
        }
        /* Colour conversion, vectorised. */
        uint32_t *o = dst + (intptr_t)j * ds;
        for (int i = 0; i < dw; i++) {
            int yy = (yb[i] - yoff) * ymul, uu = ub[i], vv = vb[i];
            int r = (yy + cr_r * vv + 512) >> 10;
            int g = (yy - cb_g * uu - cr_g * vv + 512) >> 10;
            int b = (yy + cb_b * uu + 512) >> 10;
            r = r < 0 ? 0 : r > 255 ? 255 : r;
            g = g < 0 ? 0 : g > 255 ? 255 : g;
            b = b < 0 ? 0 : b > 255 ? 255 : b;
            o[i] = 0xff000000u | ((uint32_t)r << 16) | ((uint32_t)g << 8) | (uint32_t)b;
        }
    }
}
