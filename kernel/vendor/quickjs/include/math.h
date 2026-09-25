#pragma once
#define NAN (__builtin_nan(""))
#define INFINITY (__builtin_inf())
#define HUGE_VAL (__builtin_huge_val())
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define isfinite(x) __builtin_isfinite(x)
#define signbit(x) __builtin_signbit(x)
#define fpclassify(x) __builtin_fpclassify(FP_NAN, FP_INFINITE, FP_NORMAL, FP_SUBNORMAL, FP_ZERO, x)
#define FP_NAN 0
#define FP_INFINITE 1
#define FP_ZERO 2
#define FP_SUBNORMAL 3
#define FP_NORMAL 4
double fabs(double); double floor(double); double ceil(double); double trunc(double); double round(double);
double rint(double); double nearbyint(double); long lrint(double); double fmod(double, double);
double sqrt(double); double cbrt(double); double pow(double, double); double exp(double); double expm1(double);
double log(double); double log2(double); double log10(double); double log1p(double);
double sin(double); double cos(double); double tan(double); double asin(double); double acos(double);
double atan(double); double atan2(double, double); double sinh(double); double cosh(double); double tanh(double);
double asinh(double); double acosh(double); double atanh(double); double hypot(double, double);
double copysign(double, double); double fmin(double, double); double fmax(double, double);
double ldexp(double, int); double frexp(double, int *); double modf(double, double *); double scalbn(double, int);
float fabsf(float); float sqrtf(float);
