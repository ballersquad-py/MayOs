#pragma once
#include <stddef.h>
void *malloc(size_t);
void *calloc(size_t, size_t);
void *realloc(void *, size_t);
void free(void *);
void abort(void) __attribute__((noreturn));
void exit(int) __attribute__((noreturn));
int abs(int);
long labs(long);
long long llabs(long long);
double strtod(const char *, char **);
long strtol(const char *, char **, int);
unsigned long strtoul(const char *, char **, int);
long long strtoll(const char *, char **, int);
unsigned long long strtoull(const char *, char **, int);
int atoi(const char *);
char *getenv(const char *);
void qsort(void *, size_t, size_t, int (*)(const void *, const void *));
#include <alloca.h>
size_t malloc_usable_size(void *);
