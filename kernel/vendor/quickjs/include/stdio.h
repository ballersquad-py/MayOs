#pragma once
#include <stddef.h>
#include <stdarg.h>
typedef struct FILE FILE;
extern FILE *stdout, *stderr, *stdin;
#define EOF (-1)
int printf(const char *, ...);
int fprintf(FILE *, const char *, ...);
int vfprintf(FILE *, const char *, va_list);
int snprintf(char *, size_t, const char *, ...);
int vsnprintf(char *, size_t, const char *, va_list);
int sprintf(char *, const char *, ...);
int putchar(int);
int fputc(int, FILE *);
int putc(int, FILE *);
int fputs(const char *, FILE *);
int puts(const char *);
size_t fwrite(const void *, size_t, size_t, FILE *);
int fflush(FILE *);
