/* The bits of a C library QuickJS needs that are easiest in C (varargs).
 * Memory, time and maths come from the Rust side of the kernel. */
#include <stddef.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

void mayos_log(const char *s, size_t n);

FILE *stdout = (FILE *)1, *stderr = (FILE *)2, *stdin = (FILE *)0;
int errno;

size_t strlen(const char *s) { size_t n = 0; while (s[n]) n++; return n; }
size_t strnlen(const char *s, size_t m) { size_t n = 0; while (n < m && s[n]) n++; return n; }
int strcmp(const char *a, const char *b) { while (*a && *a == *b) { a++; b++; } return (unsigned char)*a - (unsigned char)*b; }
int strncmp(const char *a, const char *b, size_t n) { for (; n; n--, a++, b++) { if (*a != *b || !*a) return (unsigned char)*a - (unsigned char)*b; } return 0; }
char *strchr(const char *s, int c) { for (;; s++) { if (*s == (char)c) return (char *)s; if (!*s) return 0; } }
char *strrchr(const char *s, int c) { const char *r = 0; for (;; s++) { if (*s == (char)c) r = s; if (!*s) return (char *)r; } }
void *memchr(const void *p, int c, size_t n) { const unsigned char *s = p; for (; n; n--, s++) if (*s == (unsigned char)c) return (void *)s; return 0; }
char *strstr(const char *h, const char *n) { size_t l = strlen(n); if (!l) return (char *)h; for (; *h; h++) if (!strncmp(h, n, l)) return (char *)h; return 0; }
int abs(int x) { return x < 0 ? -x : x; }

/* A small vsnprintf: %d %i %u %x %X %o %c %s %p %% with flags "-0+ ",
 * width, precision and the l/ll/z/h length modifiers; %f/%e/%g print a
 * plain decimal (QuickJS formats numbers itself). */
struct out { char *buf; size_t cap, len; };
static void put(struct out *o, char c) { if (o->len + 1 < o->cap) o->buf[o->len] = c; o->len++; }

static void put_num(struct out *o, unsigned long long v, int neg, int base, int upper, int width, int prec, int zero, int left, char sign) {
    char tmp[32]; int n = 0;
    const char *dig = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    do { tmp[n++] = dig[v % base]; v /= base; } while (v);
    while (n < prec) tmp[n++] = '0';
    char s = neg ? '-' : sign;
    int total = n + (s ? 1 : 0);
    if (!left && !zero) while (width-- > total) put(o, ' ');
    if (s) put(o, s);
    if (!left && zero) while (width-- > total) put(o, '0');
    while (n) put(o, tmp[--n]);
    if (left) while (width-- > total) put(o, ' ');
}

int vsnprintf(char *buf, size_t cap, const char *f, va_list ap) {
    struct out o = { buf, cap, 0 };
    for (; *f; f++) {
        if (*f != '%') { put(&o, *f); continue; }
        f++;
        int left = 0, zero = 0; char sign = 0;
        for (;; f++) {
            if (*f == '-') left = 1; else if (*f == '0') zero = 1; else if (*f == '+') sign = '+'; else if (*f == ' ') { if (!sign) sign = ' '; } else if (*f == '#') {} else break;
        }
        int width = 0, prec = -1;
        if (*f == '*') { width = va_arg(ap, int); f++; } else while (*f >= '0' && *f <= '9') width = width * 10 + (*f++ - '0');
        if (*f == '.') { f++; prec = 0; if (*f == '*') { prec = va_arg(ap, int); f++; } else while (*f >= '0' && *f <= '9') prec = prec * 10 + (*f++ - '0'); }
        int lng = 0;
        while (*f == 'l' || *f == 'z' || *f == 'h' || *f == 'j' || *f == 't') { if (*f == 'l' || *f == 'z' || *f == 'j' || *f == 't') lng++; f++; }
        switch (*f) {
        case 'd': case 'i': {
            long long v = lng ? va_arg(ap, long long) : va_arg(ap, int);
            put_num(&o, v < 0 ? -(unsigned long long)v : (unsigned long long)v, v < 0, 10, 0, width, prec, zero, left, sign);
            break;
        }
        case 'u': case 'x': case 'X': case 'o': {
            unsigned long long v = lng ? va_arg(ap, unsigned long long) : va_arg(ap, unsigned int);
            put_num(&o, v, 0, *f == 'u' ? 10 : *f == 'o' ? 8 : 16, *f == 'X', width, prec, zero, left, 0);
            break;
        }
        case 'p': put(&o, '0'); put(&o, 'x'); put_num(&o, (uintptr_t)va_arg(ap, void *), 0, 16, 0, 0, -1, 0, 0, 0); break;
        case 'c': put(&o, (char)va_arg(ap, int)); break;
        case 's': {
            const char *s = va_arg(ap, const char *); if (!s) s = "(null)";
            int n = (int)strnlen(s, prec < 0 ? (size_t)-1 : (size_t)prec);
            if (!left) while (width-- > n) put(&o, ' ');
            for (int i = 0; i < n; i++) put(&o, s[i]);
            if (left) while (width-- > n) put(&o, ' ');
            break;
        }
        case 'f': case 'e': case 'g': case 'F': case 'E': case 'G': {
            double d = va_arg(ap, double);
            int neg = d < 0; if (neg) d = -d;
            unsigned long long ip = (unsigned long long)d;
            put_num(&o, ip, neg, 10, 0, 0, -1, 0, 0, sign);
            int p = prec < 0 ? 6 : prec;
            if (*f == 'g' || *f == 'G') p = 0;
            if (p) {
                put(&o, '.');
                double frac = d - (double)ip;
                for (int i = 0; i < p && i < 17; i++) { frac *= 10; int dd = (int)frac; put(&o, '0' + dd); frac -= dd; }
            }
            break;
        }
        case '%': put(&o, '%'); break;
        default: put(&o, '%'); if (*f) put(&o, *f); else f--; break;
        }
    }
    if (cap) buf[o.len < cap ? o.len : cap - 1] = 0;
    return (int)o.len;
}

int snprintf(char *b, size_t n, const char *f, ...) { va_list ap; va_start(ap, f); int r = vsnprintf(b, n, f, ap); va_end(ap); return r; }
int sprintf(char *b, const char *f, ...) { va_list ap; va_start(ap, f); int r = vsnprintf(b, (size_t)-1 >> 1, f, ap); va_end(ap); return r; }
int vfprintf(FILE *fp, const char *f, va_list ap) { char tmp[512]; int n = vsnprintf(tmp, sizeof tmp, f, ap); (void)fp; mayos_log(tmp, n < (int)sizeof tmp ? (size_t)n : sizeof tmp - 1); return n; }
int fprintf(FILE *fp, const char *f, ...) { va_list ap; va_start(ap, f); int r = vfprintf(fp, f, ap); va_end(ap); return r; }
int printf(const char *f, ...) { va_list ap; va_start(ap, f); int r = vfprintf(stdout, f, ap); va_end(ap); return r; }
int fputc(int c, FILE *fp) { char ch = (char)c; (void)fp; mayos_log(&ch, 1); return c; }
int putc(int c, FILE *fp) { return fputc(c, fp); }
int putchar(int c) { return fputc(c, stdout); }
int fputs(const char *s, FILE *fp) { (void)fp; mayos_log(s, strlen(s)); return 0; }
int puts(const char *s) { fputs(s, stdout); fputc('\n', stdout); return 0; }
size_t fwrite(const void *p, size_t s, size_t n, FILE *fp) { (void)fp; mayos_log(p, s * n); return n; }
int fflush(FILE *fp) { (void)fp; return 0; }
