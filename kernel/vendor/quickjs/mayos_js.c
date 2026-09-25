/* Glue between QuickJS and the MayOS browser. The DOM itself is written in
 * JavaScript (see src/gui/js/prelude.js); these natives give it access to
 * the document, which the Rust side owns. Nodes are plain integer ids. */
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include "quickjs.h"

/* Implemented in Rust (src/gui/js.rs). Strings are UTF-8, not NUL-terminated
 * on the way out (length given). `rs_get` writes up to `cap` bytes and returns
 * the full length, or -1 for "null". */
extern void mayos_log(const char *s, size_t n);
extern int rs_query(int root, const char *sel, int all, int32_t *out, int max);
extern int rs_create(const char *tag, int text);
extern long rs_get(int id, int what, const char *name, char *out, long cap);
extern void rs_set(int id, int what, const char *name, const char *value, long vlen);
extern int rs_rel(int id, int rel);
extern int rs_children(int id, int32_t *out, int max);
extern void rs_insert(int parent, int child, int before);
extern void rs_remove(int id);
extern int rs_special(int which);
extern void rs_call(int op, const char *a, long alen, const char *b, long blen);
extern long rs_ask(int op, const char *a, long alen, char *out, long cap);
extern double rs_now(void);
extern void rs_box(int id, int32_t *out4);
extern int rs_interrupt(void);

typedef struct MJS { JSRuntime *rt; JSContext *ctx; } MJS;

static const char *arg_str(JSContext *ctx, JSValueConst v, size_t *len) {
    return JS_ToCStringLen(ctx, len, v);
}

static int arg_int(JSContext *ctx, JSValueConst v) {
    int32_t i = 0;
    JS_ToInt32(ctx, &i, v);
    return i;
}

static JSValue ids_array(JSContext *ctx, int32_t *ids, int n) {
    JSValue a = JS_NewArray(ctx);
    for (int i = 0; i < n; i++) JS_SetPropertyUint32(ctx, a, i, JS_NewInt32(ctx, ids[i]));
    return a;
}

/* __n.query(root, selector, all) -> [ids] */
static JSValue n_query(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    size_t len;
    const char *sel = arg_str(ctx, argv[1], &len);
    if (!sel) return JS_EXCEPTION;
    int max = 4096;
    int32_t *ids = malloc(sizeof(int32_t) * max);
    int n = rs_query(arg_int(ctx, argv[0]), sel, JS_ToBool(ctx, argv[2]), ids, max);
    JS_FreeCString(ctx, sel);
    JSValue r = ids_array(ctx, ids, n < max ? n : max);
    free(ids);
    return r;
}

static JSValue n_create(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    size_t len;
    const char *tag = arg_str(ctx, argv[0], &len);
    if (!tag) return JS_EXCEPTION;
    int id = rs_create(tag, JS_ToBool(ctx, argv[1]));
    JS_FreeCString(ctx, tag);
    return JS_NewInt32(ctx, id);
}

/* __n.get(id, what, name) -> string | null */
static JSValue n_get(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    size_t len;
    const char *name = argc > 2 ? arg_str(ctx, argv[2], &len) : NULL;
    int id = arg_int(ctx, argv[0]), what = arg_int(ctx, argv[1]);
    char small[256];
    long n = rs_get(id, what, name ? name : "", small, sizeof small);
    JSValue r;
    if (n < 0) {
        r = JS_NULL;
    } else if (n <= (long)sizeof small) {
        r = JS_NewStringLen(ctx, small, n);
    } else {
        char *big = malloc(n);
        rs_get(id, what, name ? name : "", big, n);
        r = JS_NewStringLen(ctx, big, n);
        free(big);
    }
    if (name) JS_FreeCString(ctx, name);
    return r;
}

/* __n.set(id, what, name, value) */
static JSValue n_set(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    size_t nl, vl;
    const char *name = arg_str(ctx, argv[2], &nl);
    const char *val = JS_IsNull(argv[3]) || JS_IsUndefined(argv[3]) ? NULL : arg_str(ctx, argv[3], &vl);
    if (!name) return JS_EXCEPTION;
    rs_set(arg_int(ctx, argv[0]), arg_int(ctx, argv[1]), name, val, val ? (long)vl : -1);
    JS_FreeCString(ctx, name);
    if (val) JS_FreeCString(ctx, val);
    return JS_UNDEFINED;
}

static JSValue n_rel(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    return JS_NewInt32(ctx, rs_rel(arg_int(ctx, argv[0]), arg_int(ctx, argv[1])));
}

static JSValue n_children(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    int32_t ids[1024];
    int n = rs_children(arg_int(ctx, argv[0]), ids, 1024);
    return ids_array(ctx, ids, n < 1024 ? n : 1024);
}

static JSValue n_insert(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    rs_insert(arg_int(ctx, argv[0]), arg_int(ctx, argv[1]), arg_int(ctx, argv[2]));
    return JS_UNDEFINED;
}

static JSValue n_remove(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    rs_remove(arg_int(ctx, argv[0]));
    return JS_UNDEFINED;
}

static JSValue n_special(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    return JS_NewInt32(ctx, rs_special(arg_int(ctx, argv[0])));
}

/* __n.call(op, a, b): fire-and-forget requests (log, navigate, alert...) */
static JSValue n_call(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    size_t al = 0, bl = 0;
    const char *a = argc > 1 ? arg_str(ctx, argv[1], &al) : NULL;
    const char *b = argc > 2 ? arg_str(ctx, argv[2], &bl) : NULL;
    rs_call(arg_int(ctx, argv[0]), a ? a : "", (long)al, b ? b : "", (long)bl);
    if (a) JS_FreeCString(ctx, a);
    if (b) JS_FreeCString(ctx, b);
    return JS_UNDEFINED;
}

/* __n.ask(op, a) -> string | null (location, storage reads...) */
static JSValue n_ask(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    size_t al = 0;
    const char *a = argc > 1 ? arg_str(ctx, argv[1], &al) : NULL;
    int op = arg_int(ctx, argv[0]);
    char small[512];
    long n = rs_ask(op, a ? a : "", (long)al, small, sizeof small);
    JSValue r;
    if (n < 0) r = JS_NULL;
    else if (n <= (long)sizeof small) r = JS_NewStringLen(ctx, small, n);
    else {
        char *big = malloc(n);
        rs_ask(op, a ? a : "", (long)al, big, n);
        r = JS_NewStringLen(ctx, big, n);
        free(big);
    }
    if (a) JS_FreeCString(ctx, a);
    return r;
}

static JSValue n_now(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    return JS_NewFloat64(ctx, rs_now());
}

static JSValue n_box(JSContext *ctx, JSValueConst this_val, int argc, JSValueConst *argv) {
    int32_t b[4];
    rs_box(arg_int(ctx, argv[0]), b);
    return ids_array(ctx, b, 4);
}

static const JSCFunctionListEntry natives[] = {
    JS_CFUNC_DEF("query", 3, n_query),
    JS_CFUNC_DEF("create", 2, n_create),
    JS_CFUNC_DEF("get", 3, n_get),
    JS_CFUNC_DEF("set", 4, n_set),
    JS_CFUNC_DEF("rel", 2, n_rel),
    JS_CFUNC_DEF("children", 1, n_children),
    JS_CFUNC_DEF("insert", 3, n_insert),
    JS_CFUNC_DEF("remove", 1, n_remove),
    JS_CFUNC_DEF("special", 1, n_special),
    JS_CFUNC_DEF("call", 3, n_call),
    JS_CFUNC_DEF("ask", 2, n_ask),
    JS_CFUNC_DEF("now", 0, n_now),
    JS_CFUNC_DEF("box", 1, n_box),
};

static int interrupt(JSRuntime *rt, void *opaque) {
    return rs_interrupt();
}

MJS *mjs_new(void) {
    MJS *m = malloc(sizeof *m);
    m->rt = JS_NewRuntime();
    JS_SetMemoryLimit(m->rt, 256 * 1024 * 1024);
    JS_SetMaxStackSize(m->rt, 0);
    JS_SetInterruptHandler(m->rt, interrupt, NULL);
    m->ctx = JS_NewContext(m->rt);
    JSValue g = JS_GetGlobalObject(m->ctx);
    JSValue n = JS_NewObject(m->ctx);
    JS_SetPropertyFunctionList(m->ctx, n, natives, sizeof natives / sizeof natives[0]);
    JS_SetPropertyStr(m->ctx, g, "__n", n);
    JS_FreeValue(m->ctx, g);
    return m;
}

void mjs_free(MJS *m) {
    JS_FreeContext(m->ctx);
    JS_FreeRuntime(m->rt);
    free(m);
}

static void report(JSContext *ctx) {
    JSValue e = JS_GetException(ctx);
    const char *s = JS_ToCString(ctx, e);
    JSValue stack = JS_IsObject(e) ? JS_GetPropertyStr(ctx, e, "stack") : JS_UNDEFINED;
    const char *st = JS_IsUndefined(stack) ? NULL : JS_ToCString(ctx, stack);
    char line[600];
    int n = snprintf(line, sizeof line, "js error: %s%s%s", s ? s : "?", st ? "\n" : "", st ? st : "");
    rs_call(0, line, n < (int)sizeof line ? n : (int)sizeof line - 1, "error", 5);
    if (s) JS_FreeCString(ctx, s);
    if (st) JS_FreeCString(ctx, st);
    JS_FreeValue(ctx, stack);
    JS_FreeValue(ctx, e);
}

static void run_jobs(MJS *m) {
    JSContext *c;
    for (int i = 0; i < 10000; i++) {
        int r = JS_ExecutePendingJob(m->rt, &c);
        if (r <= 0) {
            if (r < 0) report(c);
            break;
        }
    }
}

/* Evaluate a script; returns 0 on success. */
int mjs_eval(MJS *m, const char *src, size_t len, const char *file) {
    /* QuickJS wants a NUL after the source. */
    char *copy = malloc(len + 1);
    memcpy(copy, src, len);
    copy[len] = 0;
    JSValue r = JS_Eval(m->ctx, copy, len, file, JS_EVAL_TYPE_GLOBAL);
    free(copy);
    int bad = JS_IsException(r);
    if (bad) report(m->ctx);
    JS_FreeValue(m->ctx, r);
    run_jobs(m);
    return bad;
}

/* Evaluate and return the result as a string (for the console). */
long mjs_eval_string(MJS *m, const char *src, size_t len, char *out, long cap) {
    char *copy = malloc(len + 1);
    memcpy(copy, src, len);
    copy[len] = 0;
    JSValue r = JS_Eval(m->ctx, copy, len, "<console>", JS_EVAL_TYPE_GLOBAL);
    free(copy);
    if (JS_IsException(r)) {
        JSValue e = JS_GetException(m->ctx);
        r = e;
    }
    size_t n = 0;
    const char *s = JS_ToCStringLen(m->ctx, &n, r);
    long k = 0;
    if (s) {
        k = (long)n < cap ? (long)n : cap;
        memcpy(out, s, k);
        JS_FreeCString(m->ctx, s);
    }
    JS_FreeValue(m->ctx, r);
    run_jobs(m);
    return k;
}

/* Call a global function `name` with (int, string, int, int, string). */
int mjs_call(MJS *m, const char *name, int a, const char *s, int b, int c, const char *s2) {
    JSValue g = JS_GetGlobalObject(m->ctx);
    JSValue f = JS_GetPropertyStr(m->ctx, g, name);
    int ret = 0;
    if (JS_IsFunction(m->ctx, f)) {
        JSValue args[5] = { JS_NewInt32(m->ctx, a), JS_NewString(m->ctx, s ? s : ""), JS_NewInt32(m->ctx, b), JS_NewInt32(m->ctx, c), JS_NewString(m->ctx, s2 ? s2 : "") };
        JSValue r = JS_Call(m->ctx, f, g, 5, args);
        if (JS_IsException(r)) report(m->ctx);
        else { int32_t v = 0; JS_ToInt32(m->ctx, &v, r); ret = v; }
        JS_FreeValue(m->ctx, r);
        for (int i = 0; i < 5; i++) JS_FreeValue(m->ctx, args[i]);
    }
    JS_FreeValue(m->ctx, f);
    JS_FreeValue(m->ctx, g);
    run_jobs(m);
    return ret;
}
