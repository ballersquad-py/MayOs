/* gldemo: GPU drawing you can watch. Renders with Mesa on the GPU
 * (needs /etc/gpu3d) and shows the frames in a MayOS window (/dev/fb0).
 *   gldemo triangle   - spinning RGB triangle
 *   gldemo shaders    - four animated fragment shaders
 * Add -sw to render in software instead, for comparison. Ctrl+C quits. */
#include <dlfcn.h>
#include <fcntl.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

/* Only the part of fb_var_screeninfo we read (linux/fb.h). */
struct fb_var_screeninfo { uint32_t xres, yres, rest[38]; };
#define FBIOGET_VSCREENINFO 0x4600

static void *(*egl_proc)(const char *);
#define GLF(ret, name, ...) typedef ret (*name##_t)(__VA_ARGS__); static name##_t name;
GLF(const unsigned char *, glGetString, unsigned)
GLF(void, glGenFramebuffers, int, unsigned *) GLF(void, glBindFramebuffer, unsigned, unsigned)
GLF(void, glGenRenderbuffers, int, unsigned *) GLF(void, glBindRenderbuffer, unsigned, unsigned)
GLF(void, glRenderbufferStorage, unsigned, unsigned, int, int)
GLF(void, glFramebufferRenderbuffer, unsigned, unsigned, unsigned, unsigned)
GLF(void, glViewport, int, int, int, int) GLF(void, glClearColor, float, float, float, float) GLF(void, glClear, unsigned)
GLF(void, glReadPixels, int, int, int, int, unsigned, unsigned, void *)
GLF(unsigned, glCreateShader, unsigned) GLF(void, glShaderSource, unsigned, int, const char **, const int *)
GLF(void, glCompileShader, unsigned) GLF(unsigned, glCreateProgram, void) GLF(void, glAttachShader, unsigned, unsigned)
GLF(void, glLinkProgram, unsigned) GLF(void, glUseProgram, unsigned) GLF(void, glGetShaderInfoLog, unsigned, int, int *, char *)
GLF(void, glGetShaderiv, unsigned, unsigned, int *)
GLF(void, glVertexAttribPointer, unsigned, int, unsigned, unsigned char, int, const void *)
GLF(void, glEnableVertexAttribArray, unsigned) GLF(void, glDrawArrays, unsigned, int, int)
GLF(void, glBindAttribLocation, unsigned, unsigned, const char *)
GLF(int, glGetUniformLocation, unsigned, const char *) GLF(void, glUniform1f, int, float) GLF(void, glUniform2f, int, float, float)

static int init_gl(int sw) {
    typedef void *(*gbm_create_device_t)(int);
    typedef void *(*eglGetPlatformDisplay_t)(unsigned, void *, const intptr_t *);
    typedef unsigned (*eglInitialize_t)(void *, int *, int *);
    typedef unsigned (*eglBindAPI_t)(unsigned);
    typedef void *(*eglCreateContext_t)(void *, void *, void *, const int *);
    typedef unsigned (*eglMakeCurrent_t)(void *, void *, void *, void *);
    if (!sw) {
        unsetenv("LIBGL_ALWAYS_SOFTWARE");
        unsetenv("GALLIUM_DRIVER");
    }
    void *gbm = dlopen("libgbm.so.1", RTLD_NOW | RTLD_GLOBAL), *egl = dlopen("libEGL.so.1", RTLD_NOW | RTLD_GLOBAL);
    if (!gbm || !egl) { printf("cannot load Mesa (install Firefox first): %s\n", dlerror()); return 0; }
    gbm_create_device_t gbm_create_device = (gbm_create_device_t)dlsym(gbm, "gbm_create_device");
    eglGetPlatformDisplay_t getdpy = (eglGetPlatformDisplay_t)dlsym(egl, "eglGetPlatformDisplay");
    eglInitialize_t init = (eglInitialize_t)dlsym(egl, "eglInitialize");
    eglBindAPI_t bind = (eglBindAPI_t)dlsym(egl, "eglBindAPI");
    eglCreateContext_t create = (eglCreateContext_t)dlsym(egl, "eglCreateContext");
    eglMakeCurrent_t make = (eglMakeCurrent_t)dlsym(egl, "eglMakeCurrent");
    egl_proc = (void *(*)(const char *))dlsym(egl, "eglGetProcAddress");
    void *dpy;
    if (sw) {
        dpy = getdpy(0x31DD, 0, 0);
    } else {
        int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
        if (fd < 0) { printf("no GPU device: run touch /etc/gpu3d and reboot\n"); return 0; }
        dpy = getdpy(0x31D7, gbm_create_device(fd), 0);
    }
    int a, b;
    if (!dpy || !init(dpy, &a, &b)) { printf("EGL failed\n"); return 0; }
    bind(0x30A0);
    int cattrs[] = {0x3098, 2, 0x3038};
    void *ctx = create(dpy, 0, 0, cattrs);
    if (!ctx || !make(dpy, 0, 0, ctx)) { printf("no GL context\n"); return 0; }
#define L(name) name = (name##_t)egl_proc(#name);
    L(glGetString) L(glGenFramebuffers) L(glBindFramebuffer) L(glGenRenderbuffers) L(glBindRenderbuffer)
    L(glRenderbufferStorage) L(glFramebufferRenderbuffer) L(glViewport) L(glClearColor) L(glClear) L(glReadPixels)
    L(glCreateShader) L(glShaderSource) L(glCompileShader) L(glCreateProgram) L(glAttachShader) L(glLinkProgram)
    L(glUseProgram) L(glGetShaderInfoLog) L(glGetShaderiv) L(glVertexAttribPointer) L(glEnableVertexAttribArray)
    L(glDrawArrays) L(glBindAttribLocation) L(glGetUniformLocation) L(glUniform1f) L(glUniform2f)
    printf("Rendering with: %s\n", glGetString(0x1F01));
    return 1;
}

static unsigned shader(unsigned type, const char *src) {
    unsigned s = glCreateShader(type);
    glShaderSource(s, 1, &src, 0);
    glCompileShader(s);
    int ok = 0;
    glGetShaderiv(s, 0x8B81, &ok);
    if (!ok) { char log[1024]; glGetShaderInfoLog(s, sizeof log, 0, log); printf("shader error: %s\n", log); }
    return s;
}

static unsigned program(const char *vs, const char *fs) {
    unsigned p = glCreateProgram();
    glAttachShader(p, shader(0x8B31, vs));
    glAttachShader(p, shader(0x8B30, fs));
    glBindAttribLocation(p, 0, "pos");
    glBindAttribLocation(p, 1, "col");
    glLinkProgram(p);
    return p;
}

static const char *QUAD_VS =
    "attribute vec2 pos; varying vec2 uv; void main() { uv = pos * 0.5 + 0.5; gl_Position = vec4(pos, 0.0, 1.0); }";
static const char *FS_HEAD = "precision highp float; varying vec2 uv; uniform float t; uniform vec2 res;\n";
static const char *FS[4] = {
    // plasma
    "void main() { vec2 p = uv * 8.0; float v = sin(p.x + t) + sin(p.y + t * 1.3) + sin(p.x + p.y + t * 0.7) + sin(length(p - 4.0) * 1.5 - t * 2.0);"
    " gl_FragColor = vec4(0.5 + 0.5 * sin(v * 3.14159 + vec3(0.0, 2.09, 4.18)), 1.0); }",
    // mandelbrot zoom
    "void main() { float z = exp(-mod(t * 0.35, 9.0)); vec2 c = vec2(-0.7453, 0.1127) + (uv - 0.5) * 3.0 * z; vec2 q = vec2(0.0); float n = 0.0;"
    " for (int i = 0; i < 160; i++) { q = vec2(q.x * q.x - q.y * q.y, 2.0 * q.x * q.y) + c; if (dot(q, q) > 4.0) break; n += 1.0; }"
    " float k = n / 160.0; gl_FragColor = vec4((0.5 + 0.5 * cos(6.2831 * (k * 3.0 + vec3(0.0, 0.33, 0.67)))) * step(k, 0.999), 1.0); }",
    // tunnel
    "void main() { vec2 p = (uv - 0.5) * vec2(res.x / res.y, 1.0); float a = atan(p.y, p.x); float r = length(p) + 0.001;"
    " vec2 tc = vec2(a / 3.14159 + t * 0.1, 0.3 / r + t * 0.6); float c = step(0.5, fract(tc.x * 6.0)) * 0.6 + step(0.5, fract(tc.y * 4.0)) * 0.4;"
    " gl_FragColor = vec4(vec3(c * r * 2.0) * vec3(0.3, 0.7, 1.0), 1.0); }",
    // raymarched spheres
    "float map(vec3 p) { vec3 q = mod(p, 2.0) - 1.0; return length(q) - 0.35; }"
    " void main() { vec2 p = (uv - 0.5) * vec2(res.x / res.y, 1.0) * 2.0; vec3 ro = vec3(sin(t * 0.3), cos(t * 0.2), t);"
    " vec3 rd = normalize(vec3(p, 1.5)); float d = 0.0; float h = 1.0; for (int i = 0; i < 64; i++) { h = map(ro + rd * d); if (h < 0.001) break; d += h; }"
    " vec3 hp = ro + rd * d; vec2 e = vec2(0.001, 0.0); vec3 n = normalize(vec3(map(hp + e.xyy) - map(hp - e.xyy), map(hp + e.yxy) - map(hp - e.yxy), map(hp + e.yyx) - map(hp - e.yyx)));"
    " float light = max(dot(n, normalize(vec3(0.5, 0.8, -0.6))), 0.0); vec3 col = (0.2 + 0.8 * light) * (0.5 + 0.5 * cos(hp.z + vec3(0.0, 2.0, 4.0))); col *= exp(-0.08 * d);"
    " gl_FragColor = vec4(col, 1.0); }",
};

/* A 3x5 pixel font for the fps counter: digits, '.', ' ', 'F', 'P', 'S'. */
static const char *GLYPHS = "0123456789. FPS";
static const uint16_t FONT[] = {
    0x7B6F, 0x2492, 0x73E7, 0x73CF, 0x5BC9, 0x79CF, 0x79EF, 0x7249, 0x7BEF, 0x7BCF, 0x0002, 0x0000, 0x79A4, 0x7BE4, 0x79CF,
};

/* Draw text at (x, y) with each font pixel as a `sc` x `sc` block, on a
 * dark box so it stays readable over any picture. */
static void draw_text(uint32_t *scr, int W, int H, int x, int y, int sc, const char *t) {
    int n = strlen(t);
    for (int yy = y - sc; yy < y + 6 * sc && yy < H; yy++)
        for (int xx = x - sc; xx < x + n * 4 * sc && xx < W; xx++)
            if (yy >= 0 && xx >= 0) scr[yy * W + xx] = 0x101010;
    for (int i = 0; i < n; i++) {
        const char *g = strchr(GLYPHS, t[i]);
        if (!g) continue;
        uint16_t bits = FONT[g - GLYPHS];
        for (int r = 0; r < 5; r++)
            for (int c = 0; c < 3; c++)
                if (bits & (1 << (14 - (r * 3 + c))))
                    for (int dy = 0; dy < sc; dy++)
                        for (int dx = 0; dx < sc; dx++) {
                            int px = x + (i * 4 + c) * sc + dx, py = y + r * sc + dy;
                            if (px < W && py < H) scr[py * W + px] = 0x40ff40;
                        }
    }
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "";
    int sw = argc > 2 && !strcmp(argv[2], "-sw");
    int tri = !strcmp(mode, "triangle"), shaders = !strcmp(mode, "shaders");
    if (!tri && !shaders) { printf("usage: gldemo triangle|shaders [-sw]\n"); return 1; }
    int fb = open("/dev/fb0", O_RDWR);
    struct fb_var_screeninfo vi;
    if (fb < 0 || ioctl(fb, FBIOGET_VSCREENINFO, &vi)) { printf("no window (/dev/fb0)\n"); return 1; }
    int W = vi.xres, H = vi.yres;
    uint32_t *screen = mmap(0, W * H * 4, PROT_READ | PROT_WRITE, MAP_SHARED, fb, 0);
    if (screen == MAP_FAILED) { printf("cannot map the window\n"); return 1; }
    if (!init_gl(sw)) return 1;
    unsigned fbo, rb;
    glGenFramebuffers(1, &fbo); glBindFramebuffer(0x8D40, fbo);
    glGenRenderbuffers(1, &rb); glBindRenderbuffer(0x8D41, rb);
    glRenderbufferStorage(0x8D41, 0x8058, W, H);
    glFramebufferRenderbuffer(0x8D40, 0x8CE0, 0x8D41, rb);

    unsigned tri_prog = program(
        "attribute vec2 pos; attribute vec3 col; varying vec3 c; uniform float t;"
        " void main() { float s = sin(t), k = cos(t); c = col; gl_Position = vec4(pos.x * k - pos.y * s, pos.x * s + pos.y * k, 0.0, 1.0); }",
        "precision mediump float; varying vec3 c; void main() { gl_FragColor = vec4(c, 1.0); }");
    unsigned progs[4];
    for (int i = 0; i < 4; i++) {
        char src[4096];
        snprintf(src, sizeof src, "%s%s", FS_HEAD, FS[i]);
        progs[i] = program(QUAD_VS, src);
    }
    static const float quad[] = {-1, -1, 1, -1, -1, 1, 1, 1};
    static const float tri_pos[] = {0.0f, 0.8f, -0.7f, -0.45f, 0.7f, -0.45f};
    static const float tri_col[] = {1, 0, 0, 0, 1, 0, 0, 0, 1};
    uint8_t *px = malloc(W * H * 4);
    struct timespec t0, now, last;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    last = t0;
    int frames = 0;
    char fps_text[32] = "... FPS";
    printf("Drawing in the window; Ctrl+C to stop.\n");
    for (;;) {
        clock_gettime(CLOCK_MONOTONIC, &now);
        float t = (now.tv_sec - t0.tv_sec) + (now.tv_nsec - t0.tv_nsec) / 1e9f;
        if (tri) {
            glViewport(0, 0, W, H);
            glClearColor(0.08f, 0.08f, 0.12f, 1); glClear(0x4000);
            glUseProgram(tri_prog);
            glUniform1f(glGetUniformLocation(tri_prog, "t"), t);
            glVertexAttribPointer(0, 2, 0x1406, 0, 0, tri_pos); glEnableVertexAttribArray(0);
            glVertexAttribPointer(1, 3, 0x1406, 0, 0, tri_col); glEnableVertexAttribArray(1);
            glDrawArrays(4, 0, 3);
        } else {
            for (int i = 0; i < 4; i++) {
                int x = (i % 2) * W / 2, y = (1 - i / 2) * H / 2;
                glViewport(x, y, W / 2, H / 2);
                glUseProgram(progs[i]);
                glUniform1f(glGetUniformLocation(progs[i], "t"), t);
                glUniform2f(glGetUniformLocation(progs[i], "res"), W / 2, H / 2);
                glVertexAttribPointer(0, 2, 0x1406, 0, 0, quad); glEnableVertexAttribArray(0);
                glDrawArrays(5, 0, 4);
            }
        }
        glReadPixels(0, 0, W, H, 0x1908, 0x1401, px);
        // GL rows go bottom-up and RGBA; the window is top-down XRGB.
        for (int y = 0; y < H; y++) {
            const uint8_t *s = px + (size_t)(H - 1 - y) * W * 4;
            uint32_t *d = screen + (size_t)y * W;
            for (int x = 0; x < W; x++)
                d[x] = (s[x * 4] << 16) | (s[x * 4 + 1] << 8) | s[x * 4 + 2];
        }
        draw_text(screen, W, H, 12, 12, 4, fps_text);
        frames++;
        double since = (now.tv_sec - last.tv_sec) + (now.tv_nsec - last.tv_nsec) / 1e9;
        if (since >= 1.0) {
            printf("%.1f fps\n", frames / since);
            snprintf(fps_text, sizeof fps_text, "%.0f FPS", frames / since);
            fflush(stdout);
            frames = 0;
            last = now;
        }
    }
}
