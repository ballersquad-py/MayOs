/* gltest: renders with the GPU through Mesa (libgbm + libEGL + GLES2),
 * loaded at run time exactly like Firefox does. Needs /etc/gpu3d and the
 * Firefox install (which brings Mesa). Prints what Mesa picked and draws
 * a triangle offscreen, then checks pixels. */
#include <dlfcn.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

typedef void *(*gbm_create_device_t)(int);
typedef void *(*eglGetPlatformDisplay_t)(unsigned, void *, const intptr_t *);
typedef unsigned (*eglInitialize_t)(void *, int *, int *);
typedef const char *(*eglQueryString_t)(void *, int);
typedef unsigned (*eglBindAPI_t)(unsigned);
typedef unsigned (*eglChooseConfig_t)(void *, const int *, void **, int, int *);
typedef void *(*eglCreateContext_t)(void *, void *, void *, const int *);
typedef unsigned (*eglMakeCurrent_t)(void *, void *, void *, void *);
typedef void *(*eglGetProcAddress_t)(const char *);
typedef int (*eglGetError_t)(void);

#define LOAD(lib, name) name##_t name = (name##_t)dlsym(lib, #name); if (!name) { printf("FAIL %s missing\n", #name); return 1; }

int main(int argc, char **argv) {
    // Let Mesa use the GPU (MayOS forces software rendering by default)
    // and make it say what it does.
    unsetenv("LIBGL_ALWAYS_SOFTWARE");
    unsetenv("GALLIUM_DRIVER");
    setenv("EGL_LOG_LEVEL", "info", 0);
    if (argc > 1 && !strcmp(argv[1], "-v")) {
        setenv("EGL_LOG_LEVEL", "debug", 1);
        setenv("LIBGL_DEBUG", "verbose", 1);
        setenv("SVGA_DEBUG", "", 0);
    }
    int sw = 0, wl = 0, tex = 0, full = 0; // -sw: software check, -wl: through the compositor, -tex: texture upload
    for (int i = 1; i < argc; i++) {
        sw |= !strcmp(argv[i], "-sw");
        wl |= !strcmp(argv[i], "-wl");
        tex |= !strcmp(argv[i], "-tex");
        full |= !strcmp(argv[i], "-full");
    }
    int fd = sw || wl ? -1 : open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (fd < 0 && !sw && !wl) { printf("FAIL no /dev/dri/renderD128 (touch /etc/gpu3d and reboot)\n"); return 1; }
    void *gbm = dlopen("libgbm.so.1", RTLD_NOW | RTLD_GLOBAL);
    void *egl = dlopen("libEGL.so.1", RTLD_NOW | RTLD_GLOBAL);
    if (!gbm || !egl) { printf("FAIL cannot load Mesa: %s\n", dlerror()); return 1; }
    LOAD(gbm, gbm_create_device);
    LOAD(egl, eglGetPlatformDisplay); LOAD(egl, eglInitialize); LOAD(egl, eglQueryString); LOAD(egl, eglBindAPI);
    LOAD(egl, eglChooseConfig); LOAD(egl, eglCreateContext); LOAD(egl, eglMakeCurrent); LOAD(egl, eglGetProcAddress);
    LOAD(egl, eglGetError);
    void *dev = sw || wl ? 0 : gbm_create_device(fd);
    if (!sw && !wl) printf("%s gbm device\n", dev ? "ok  " : "FAIL");
    if (!dev && !sw && !wl) return 1;
    void *wldpy = 0;
    if (wl) {
        void *wlc = dlopen("libwayland-client.so.0", RTLD_NOW | RTLD_GLOBAL);
        void *(*connect)(const char *) = wlc ? (void *(*)(const char *))dlsym(wlc, "wl_display_connect") : 0;
        wldpy = connect ? connect(0) : 0;
        printf("%s Wayland connection\n", wldpy ? "ok  " : "FAIL");
        if (!wldpy) return 1;
    }
    void *dpy = wl ? eglGetPlatformDisplay(0x31D8 /* WAYLAND_KHR */, wldpy, 0) : sw ? eglGetPlatformDisplay(0x31DD /* SURFACELESS_MESA */, 0, 0) : eglGetPlatformDisplay(0x31D7 /* EGL_PLATFORM_GBM_KHR */, dev, 0);
    int maj = 0, min = 0;
    if (!dpy || !eglInitialize(dpy, &maj, &min)) { printf("FAIL eglInitialize (error %#x)\n", eglGetError()); return 1; }
    printf("ok   EGL %d.%d, vendor %s\n", maj, min, eglQueryString(dpy, 0x3053));
    eglBindAPI(0x30A0 /* EGL_OPENGL_ES_API */);
    int attrs[] = {0x3040 /* RENDERABLE_TYPE */, 0x0004 /* ES2 */, 0x3033 /* SURFACE_TYPE */, wl ? 4 : 0, 0x3038};
    void *cfg = 0; int n = 0;
    eglChooseConfig(dpy, attrs, &cfg, 1, &n);
    if (wl) {
        printf("%s window configs: %d\n", n ? "ok  " : "FAIL", n);
        typedef unsigned (*eglGetConfigs_t)(void *, void **, int, int *);
        typedef unsigned (*eglGetConfigAttrib_t)(void *, void *, int, int *);
        eglGetConfigs_t getc = (eglGetConfigs_t)dlsym(egl, "eglGetConfigs");
        eglGetConfigAttrib_t geta = (eglGetConfigAttrib_t)dlsym(egl, "eglGetConfigAttrib");
        void *all[256]; int na = 0;
        getc(dpy, all, 256, &na);
        for (int i = 0; i < na; i++) {
            int r, g, b, a2, d, st, surf, rt;
            geta(dpy, all[i], 0x3024, &r); geta(dpy, all[i], 0x3023, &g); geta(dpy, all[i], 0x3022, &b); geta(dpy, all[i], 0x3021, &a2);
            geta(dpy, all[i], 0x3025, &d); geta(dpy, all[i], 0x3026, &st); geta(dpy, all[i], 0x3033, &surf); geta(dpy, all[i], 0x3040, &rt);
            if (surf & 4) printf("     window config %d: rgba %d%d%d%d depth %d stencil %d renderable %#x\n", i, r, g, b, a2, d, st, rt);
        }
    }
    int cattrs[] = {0x3098 /* CONTEXT_CLIENT_VERSION */, full ? 3 : 2, 0x3038};
    void *ctx = eglCreateContext(dpy, n ? cfg : 0, 0, cattrs);
    if (!ctx || !eglMakeCurrent(dpy, 0, 0, ctx)) { printf("FAIL context (error %#x)\n", eglGetError()); return 1; }
#define GL(ret, name, ...) typedef ret (*name##_t)(__VA_ARGS__); name##_t name = (name##_t)eglGetProcAddress(#name);
    GL(const unsigned char *, glGetString, unsigned)
    GL(void, glGenFramebuffers, int, unsigned *) GL(void, glBindFramebuffer, unsigned, unsigned)
    GL(void, glGenRenderbuffers, int, unsigned *) GL(void, glBindRenderbuffer, unsigned, unsigned)
    GL(void, glRenderbufferStorage, unsigned, unsigned, int, int)
    GL(void, glFramebufferRenderbuffer, unsigned, unsigned, unsigned, unsigned)
    GL(unsigned, glCheckFramebufferStatus, unsigned)
    GL(void, glViewport, int, int, int, int) GL(void, glClearColor, float, float, float, float) GL(void, glClear, unsigned)
    GL(void, glReadPixels, int, int, int, int, unsigned, unsigned, void *) GL(void, glFinish, void)
    GL(unsigned, glCreateShader, unsigned) GL(void, glShaderSource, unsigned, int, const char **, const int *)
    GL(void, glCompileShader, unsigned) GL(unsigned, glCreateProgram, void) GL(void, glAttachShader, unsigned, unsigned)
    GL(void, glLinkProgram, unsigned) GL(void, glUseProgram, unsigned) GL(void, glGetProgramiv, unsigned, unsigned, int *)
    GL(void, glVertexAttribPointer, unsigned, int, unsigned, unsigned char, int, const void *)
    GL(void, glEnableVertexAttribArray, unsigned) GL(void, glDrawArrays, unsigned, int, int) GL(void, glBindAttribLocation, unsigned, unsigned, const char *)
    GL(unsigned, glGetError, void)
    const char *renderer = (const char *)glGetString(0x1F01);
    printf("ok   GL renderer: %s\n", renderer);
    printf("     GL version:  %s\n", glGetString(0x1F02));
    int hw = renderer && !strstr(renderer, "llvmpipe") && !strstr(renderer, "softpipe");
    printf("%s using the GPU (not software)\n", hw ? "ok  " : "FAIL");

    const int W = 256, H = 256;
    unsigned fb, rb;
    glGenFramebuffers(1, &fb); glBindFramebuffer(0x8D40, fb);
    glGenRenderbuffers(1, &rb); glBindRenderbuffer(0x8D41, rb);
    glRenderbufferStorage(0x8D41, 0x8058 /* RGBA8 */, W, H);
    glFramebufferRenderbuffer(0x8D40, 0x8CE0, 0x8D41, rb);
    printf("%s framebuffer\n", glCheckFramebufferStatus(0x8D40) == 0x8CD5 ? "ok  " : "FAIL");
    glViewport(0, 0, W, H);
    const char *vs = "attribute vec2 p; void main() { gl_Position = vec4(p, 0.0, 1.0); }";
    const char *fs = "precision mediump float; void main() { gl_FragColor = vec4(0.0, 1.0, 0.0, 1.0); }";
    unsigned v = glCreateShader(0x8B31), f = glCreateShader(0x8B30), prog = glCreateProgram();
    glShaderSource(v, 1, &vs, 0); glCompileShader(v);
    glShaderSource(f, 1, &fs, 0); glCompileShader(f);
    glAttachShader(prog, v); glAttachShader(prog, f); glBindAttribLocation(prog, 0, "p"); glLinkProgram(prog);
    int linked = 0; glGetProgramiv(prog, 0x8B82, &linked);
    printf("%s shaders compiled and linked\n", linked ? "ok  " : "FAIL");
    float tri[] = {-1, -1, 3, -1, -1, 3}; // covers the whole target
    struct timespec t0, t1; clock_gettime(CLOCK_MONOTONIC, &t0);
    int frames = 200;
    for (int i = 0; i < frames; i++) {
        glClearColor(1, 0, 0, 1); glClear(0x4000);
        glUseProgram(prog);
        glVertexAttribPointer(0, 2, 0x1406, 0, 0, tri); glEnableVertexAttribArray(0);
        glDrawArrays(4, 0, 3);
    }
    glFinish();
    clock_gettime(CLOCK_MONOTONIC, &t1);
    double ms = (t1.tv_sec - t0.tv_sec) * 1e3 + (t1.tv_nsec - t0.tv_nsec) / 1e6;
    static uint8_t px[256 * 256 * 4];
    glReadPixels(0, 0, W, H, 0x1908, 0x1401, px);
    int green = 0;
    for (int i = 0; i < W * H; i++) green += px[i * 4] < 10 && px[i * 4 + 1] > 245 && px[i * 4 + 2] < 10;
    printf("%s triangle drawn: %d of %d pixels green (first %d,%d,%d), GL error %#x\n", green == W * H ? "ok  " : "FAIL", green, W * H, px[0], px[1], px[2], glGetError());
    printf("     %d frames in %.1f ms (%.0f fps)\n", frames, ms, frames * 1000.0 / ms);
    if (wl) {
        // Like Firefox: a wl_surface, a wl_egl_window, an EGL window surface,
        // draw and present a few frames through the compositor.
        void *wlc = dlopen("libwayland-client.so.0", RTLD_NOW | RTLD_GLOBAL);
        void *wle = dlopen("libwayland-egl.so.1", RTLD_NOW | RTLD_GLOBAL);
        struct wl_interface_s { const char *name; int version; int mc; const void *m; int ec; const void *e; };
        const struct wl_interface_s *reg_if = dlsym(wlc, "wl_registry_interface");
        const struct wl_interface_s *comp_if = dlsym(wlc, "wl_compositor_interface");
        const struct wl_interface_s *surf_if = dlsym(wlc, "wl_surface_interface");
        void *(*marshal)(void *, uint32_t, const void *, uint32_t, uint32_t, ...) = dlsym(wlc, "wl_proxy_marshal_flags");
        uint32_t (*pver)(void *) = dlsym(wlc, "wl_proxy_get_version");
        int (*add_listener)(void *, void (**)(void), void *) = dlsym(wlc, "wl_proxy_add_listener");
        int (*roundtrip)(void *) = dlsym(wlc, "wl_display_roundtrip");
        void *(*egl_win_create)(void *, int, int) = dlsym(wle, "wl_egl_window_create");
        typedef void *(*eglCreateWindowSurface_t)(void *, void *, void *, const int *);
        typedef unsigned (*eglSwapBuffers_t)(void *, void *);
        eglCreateWindowSurface_t mkwin = (eglCreateWindowSurface_t)dlsym(egl, "eglCreateWindowSurface");
        eglSwapBuffers_t swap = (eglSwapBuffers_t)dlsym(egl, "eglSwapBuffers");
        if (!reg_if || !comp_if || !marshal || !egl_win_create || !mkwin) { printf("FAIL Wayland symbols missing\n"); return 1; }
        static void *compositor;
        struct reg_listener { void (*global)(void *, void *, uint32_t, const char *, uint32_t); void (*remove)(void *, void *, uint32_t); };
        static const struct wl_interface_s *s_comp_if;
        static void *(*s_marshal)(void *, uint32_t, const void *, uint32_t, uint32_t, ...);
        s_comp_if = comp_if; s_marshal = marshal;
        void on_global(void *d, void *reg, uint32_t name, const char *iface, uint32_t ver) {
            (void)d; (void)ver;
            if (!strcmp(iface, "wl_compositor"))
                compositor = s_marshal(reg, 0 /* bind */, s_comp_if, 4, 0, name, s_comp_if->name, 4u, (void *)0);
        }
        void on_remove(void *d, void *reg, uint32_t name) { (void)d; (void)reg; (void)name; }
        static struct reg_listener rl;
        rl.global = on_global; rl.remove = on_remove;
        void *registry = marshal(wldpy, 1 /* get_registry */, reg_if, pver(wldpy), 0, (void *)0);
        add_listener(registry, (void (**)(void))&rl, 0);
        roundtrip(wldpy);
        if (!compositor) { printf("FAIL no wl_compositor\n"); return 1; }
        void *surface = marshal(compositor, 0 /* create_surface */, surf_if, pver(compositor), 0, (void *)0);
        void *win = egl_win_create(surface, 256, 256);
        {
            intptr_t *w = win;
            printf("     wl_egl_window %p: version %ld, surface field %#lx (wl_surface %p)\n", win, (long)w[0], (long)w[7], surface);
        }
        void *esurf = mkwin(dpy, n ? cfg : 0, win, 0);
        printf("%s EGL window surface\n", esurf ? "ok  " : "FAIL");
        if (!esurf) return 1;
        typedef unsigned (*eglMakeCurrent2_t)(void *, void *, void *, void *);
        ((eglMakeCurrent2_t)dlsym(egl, "eglMakeCurrent"))(dpy, esurf, esurf, ctx);
        glBindFramebuffer(0x8D40, 0);
        int swapped = 0;
        for (int i = 0; i < 30; i++) {
            glViewport(0, 0, 256, 256);
            glClearColor(i & 1, 0.5f, 1 - (i & 1), 1);
            glClear(0x4000);
            swapped += swap(dpy, esurf) ? 1 : 0;
        }
        printf("%s presented %d of 30 frames through the compositor\n", swapped == 30 ? "ok  " : "FAIL", swapped);
        green = W * H * (swapped == 30);
        if (sw) green = green;
    }
    if (full) {
        // One check per GL feature games use; each clears, draws, reads a pixel.
        GL(void, glGenBuffers, int, unsigned *) GL(void, glBindBuffer, unsigned, unsigned)
        GL(void, glBufferData, unsigned, long, const void *, unsigned) GL(void, glBufferSubData, unsigned, long, long, const void *)
        GL(void, glDrawElements, unsigned, int, unsigned, const void *) GL(void, glEnable, unsigned) GL(void, glDisable, unsigned)
        GL(void, glDepthFunc, unsigned) GL(void, glClearDepthf, float) GL(void, glUniform4f, int, float, float, float, float)
        GL(int, glGetUniformLocation, unsigned, const char *) GL(void, glGenTextures, int, unsigned *) GL(void, glBindTexture, unsigned, unsigned)
        GL(void, glTexImage2D, unsigned, int, int, int, int, int, unsigned, unsigned, const void *)
        GL(void, glTexSubImage2D, unsigned, int, int, int, int, int, unsigned, unsigned, const void *)
        GL(void, glTexParameteri, unsigned, unsigned, int) GL(void, glFramebufferTexture2D, unsigned, unsigned, unsigned, unsigned, int)
        GL(void, glGenRenderbuffers, int, unsigned *)
        const char *vs2 = "attribute vec3 p; void main() { gl_Position = vec4(p, 1.0); }";
        const char *fs2 = "precision mediump float; uniform vec4 c; void main() { gl_FragColor = c; }";
        unsigned a = glCreateShader(0x8B31), b = glCreateShader(0x8B30), pr = glCreateProgram();
        glShaderSource(a, 1, &vs2, 0); glCompileShader(a); glShaderSource(b, 1, &fs2, 0); glCompileShader(b);
        glAttachShader(pr, a); glAttachShader(pr, b); glBindAttribLocation(pr, 0, "p"); glLinkProgram(pr);
        glUseProgram(pr);
        int uc = glGetUniformLocation(pr, "c");
        glBindFramebuffer(0x8D40, fb);
        glViewport(0, 0, W, H);
        int passed = 0, total = 0;
#define PIX(x, y) (px + (((y) * W + (x)) * 4))
#define CHECK_PIXEL(name, x, y, r, g, b_) do { \
            glReadPixels(0, 0, W, H, 0x1908, 0x1401, px); uint8_t *q = PIX(x, y); total++; \
            int okp = abs(q[0] - (r)) < 8 && abs(q[1] - (g)) < 8 && abs(q[2] - (b_)) < 8; passed += okp; \
            printf("%s %-34s pixel (%d,%d,%d) want (%d,%d,%d)\n", okp ? "ok  " : "FAIL", name, q[0], q[1], q[2], r, g, b_); } while (0)
        float full_quad[] = {-1, -1, 0, 1, -1, 0, -1, 1, 0, 1, 1, 0};
        unsigned vbo, ibo;
        // 1: vertex buffer object, static
        glGenBuffers(1, &vbo); glBindBuffer(0x8892, vbo);
        glBufferData(0x8892, sizeof full_quad, full_quad, 0x88E4);
        glVertexAttribPointer(0, 3, 0x1406, 0, 0, 0); glEnableVertexAttribArray(0);
        glClearColor(0, 0, 0, 1); glClear(0x4000);
        glUniform4f(uc, 1, 0, 0, 1); glDrawArrays(5, 0, 4);
        CHECK_PIXEL("vertex buffer (static)", 128, 128, 255, 0, 0);
        // 2: glBufferSubData update: shrink the quad to the left half
        float left[] = {-1, -1, 0, 0, -1, 0, -1, 1, 0, 0, 1, 0};
        glBufferSubData(0x8892, 0, sizeof left, left);
        glClear(0x4000); glUniform4f(uc, 0, 1, 0, 1); glDrawArrays(5, 0, 4);
        CHECK_PIXEL("buffer update, inside", 64, 128, 0, 255, 0);
        CHECK_PIXEL("buffer update, outside", 192, 128, 0, 0, 0);
        // 3: orphaning (glBufferData every frame, like games do)
        for (int i = 0; i < 50; i++) {
            float x = -1 + i * 0.04f;
            float q2[] = {-1, -1, 0, x, -1, 0, -1, 1, 0, x, 1, 0};
            glBufferData(0x8892, sizeof q2, q2, 0x88E8);
            glClear(0x4000); glUniform4f(uc, 0, 0, 1, 1); glDrawArrays(5, 0, 4);
        }
        CHECK_PIXEL("buffer re-upload x50 (orphaning)", 100, 128, 0, 0, 255);
        CHECK_PIXEL("buffer re-upload, right side", 254, 128, 0, 0, 0);
        // 4: indexed draw from an index buffer
        glBufferData(0x8892, sizeof full_quad, full_quad, 0x88E4);
        unsigned short idx[] = {0, 1, 2, 2, 1, 3};
        glGenBuffers(1, &ibo); glBindBuffer(0x8893, ibo); glBufferData(0x8893, sizeof idx, idx, 0x88E4);
        glClear(0x4000); glUniform4f(uc, 1, 1, 0, 1); glDrawElements(4, 6, 0x1403, 0);
        CHECK_PIXEL("index buffer draw", 200, 50, 255, 255, 0);
        // 5: many draws with changing uniforms in one frame
        glClear(0x4000);
        for (int i = 0; i < 16; i++) {
            float x0 = -1 + i * 0.125f, x1 = x0 + 0.125f;
            float strip[] = {x0, -1, 0, x1, -1, 0, x0, 1, 0, x1, 1, 0};
            glBufferData(0x8892, sizeof strip, strip, 0x88E8);
            glUniform4f(uc, i / 15.0f, 0, 1 - i / 15.0f, 1);
            glDrawArrays(5, 0, 4);
        }
        CHECK_PIXEL("16 draws, uniform changes (first)", 4, 128, 0, 0, 255);
        CHECK_PIXEL("16 draws, uniform changes (last)", 252, 128, 255, 0, 0);
        glBindBuffer(0x8893, 0);
        glBufferData(0x8892, sizeof full_quad, full_quad, 0x88E4);
        // 6: depth test with a depth buffer
        unsigned dfb, crb, drb;
        glGenFramebuffers(1, &dfb); glBindFramebuffer(0x8D40, dfb);
        glGenRenderbuffers(1, &crb); glBindRenderbuffer(0x8D41, crb); glRenderbufferStorage(0x8D41, 0x8058, W, H);
        glFramebufferRenderbuffer(0x8D40, 0x8CE0, 0x8D41, crb);
        glGenRenderbuffers(1, &drb); glBindRenderbuffer(0x8D41, drb); glRenderbufferStorage(0x8D41, 0x81A5, W, H);
        glFramebufferRenderbuffer(0x8D40, 0x8D00, 0x8D41, drb);
        glEnable(0x0B71); glDepthFunc(0x0201); glClearDepthf(1); glClear(0x4000 | 0x100);
        float nearq[] = {-1, -1, -0.5f, 1, -1, -0.5f, -1, 1, -0.5f, 1, 1, -0.5f};
        float farq[] = {-1, -1, 0.5f, 1, -1, 0.5f, -1, 1, 0.5f, 1, 1, 0.5f};
        glBufferData(0x8892, sizeof nearq, nearq, 0x88E8); glUniform4f(uc, 0, 1, 0, 1); glDrawArrays(5, 0, 4);
        glBufferData(0x8892, sizeof farq, farq, 0x88E8); glUniform4f(uc, 1, 0, 0, 1); glDrawArrays(5, 0, 4);
        CHECK_PIXEL("depth test (near wins)", 128, 128, 0, 255, 0);
        glDisable(0x0B71);
        // 7: render to texture, then sample it
        unsigned rtex, tfb;
        glGenTextures(1, &rtex); glBindTexture(0x0DE1, rtex);
        glTexParameteri(0x0DE1, 0x2801, 0x2600); glTexParameteri(0x0DE1, 0x2800, 0x2600);
        glTexImage2D(0x0DE1, 0, 0x1908, 64, 64, 0, 0x1908, 0x1401, 0);
        glGenFramebuffers(1, &tfb); glBindFramebuffer(0x8D40, tfb); glFramebufferTexture2D(0x8D40, 0x8CE0, 0x0DE1, rtex, 0);
        glViewport(0, 0, 64, 64); glClearColor(1, 0, 1, 1); glClear(0x4000);
        glBindFramebuffer(0x8D40, fb); glViewport(0, 0, W, H); glClearColor(0, 0, 0, 1); glClear(0x4000);
        const char *tvs = "attribute vec3 p; varying vec2 uv; void main() { uv = p.xy * 0.5 + 0.5; gl_Position = vec4(p, 1.0); }";
        const char *tfs = "precision mediump float; varying vec2 uv; uniform sampler2D t; void main() { gl_FragColor = texture2D(t, uv); }";
        unsigned ta = glCreateShader(0x8B31), tb = glCreateShader(0x8B30), tp = glCreateProgram();
        glShaderSource(ta, 1, &tvs, 0); glCompileShader(ta); glShaderSource(tb, 1, &tfs, 0); glCompileShader(tb);
        glAttachShader(tp, ta); glAttachShader(tp, tb); glBindAttribLocation(tp, 0, "p"); glLinkProgram(tp);
        glUseProgram(tp); glBufferData(0x8892, sizeof full_quad, full_quad, 0x88E4); glDrawArrays(5, 0, 4);
        CHECK_PIXEL("render to texture, then sample", 128, 128, 255, 0, 255);
        // 8: big texture, partial update with glTexSubImage2D
        static uint8_t big[1024 * 1024 * 4];
        memset(big, 0x40, sizeof big);
        unsigned bt; glGenTextures(1, &bt); glBindTexture(0x0DE1, bt);
        glTexParameteri(0x0DE1, 0x2801, 0x2600); glTexParameteri(0x0DE1, 0x2800, 0x2600);
        glTexImage2D(0x0DE1, 0, 0x1908, 1024, 1024, 0, 0x1908, 0x1401, big);
        static uint8_t patch[512 * 512 * 4];
        for (int i = 0; i < 512 * 512; i++) { patch[i * 4] = 0; patch[i * 4 + 1] = 200; patch[i * 4 + 2] = 255; patch[i * 4 + 3] = 255; }
        glTexSubImage2D(0x0DE1, 0, 512, 512, 512, 512, 0x1908, 0x1401, patch);
        glClear(0x4000); glDrawArrays(5, 0, 4);
        CHECK_PIXEL("1024 texture, updated corner", 200, 200, 0, 200, 255);
        CHECK_PIXEL("1024 texture, untouched part", 50, 50, 0x40, 0x40, 0x40);
        // 9: a 3 MiB vertex buffer (GPU memory described by a two-level
        // page table), the quad stored at its end
        glUseProgram(pr); glBindFramebuffer(0x8D40, fb); glViewport(0, 0, W, H);
        static float bigvb[3 * 1024 * 1024 / 4];
        size_t endq = sizeof bigvb / 4 - 12;
        memcpy(bigvb + endq, full_quad, sizeof full_quad);
        unsigned bvb; glGenBuffers(1, &bvb); glBindBuffer(0x8892, bvb);
        glBufferData(0x8892, sizeof bigvb, bigvb, 0x88E4);
        glVertexAttribPointer(0, 3, 0x1406, 0, 0, (void *)(endq * 4));
        glClear(0x4000); glUniform4f(uc, 0, 1, 1, 1); glDrawArrays(5, 0, 4);
        CHECK_PIXEL("3 MiB vertex buffer, data at the end", 128, 128, 0, 255, 255);
        // 10: same, after glBufferSubData deep inside it
        float leftq[] = {-1, -1, 0, 0, -1, 0, -1, 1, 0, 0, 1, 0};
        glBufferSubData(0x8892, endq * 4, sizeof leftq, leftq);
        glClear(0x4000); glUniform4f(uc, 1, 0.5f, 0, 1); glDrawArrays(5, 0, 4);
        CHECK_PIXEL("3 MiB buffer, sub-update (inside)", 64, 128, 255, 128, 0);
        CHECK_PIXEL("3 MiB buffer, sub-update (outside)", 192, 128, 0, 0, 0);
        glVertexAttribPointer(0, 3, 0x1406, 0, 0, 0);
        // 11: mat4 uniform (camera matrix) + interleaved position/colour
        GL(void, glUniformMatrix4fv, int, int, unsigned char, const float *) GL(void, glDisableVertexAttribArray, unsigned)
        const char *mvs = "attribute vec3 p; attribute vec3 col; uniform mat4 m; varying vec3 vc; void main() { vc = col; gl_Position = m * vec4(p, 1.0); }";
        const char *mfs = "precision mediump float; varying vec3 vc; uniform vec4 tint; void main() { gl_FragColor = vec4(vc, 1.0) * tint; }";
        unsigned ma = glCreateShader(0x8B31), mb = glCreateShader(0x8B30), mp = glCreateProgram();
        glShaderSource(ma, 1, &mvs, 0); glCompileShader(ma); glShaderSource(mb, 1, &mfs, 0); glCompileShader(mb);
        glAttachShader(mp, ma); glAttachShader(mp, mb); glBindAttribLocation(mp, 0, "p"); glBindAttribLocation(mp, 1, "col"); glLinkProgram(mp);
        glUseProgram(mp);
        float inter[] = {-1, -1, 0, 1, 1, 1,  1, -1, 0, 1, 1, 1,  -1, 1, 0, 1, 1, 1,  1, 1, 0, 1, 1, 1};
        unsigned ivb; glGenBuffers(1, &ivb); glBindBuffer(0x8892, ivb); glBufferData(0x8892, sizeof inter, inter, 0x88E4);
        glVertexAttribPointer(0, 3, 0x1406, 0, 24, 0); glVertexAttribPointer(1, 3, 0x1406, 0, 24, (void *)12);
        glEnableVertexAttribArray(1);
        // scale x and y by 0.5, shift right by 0.5: covers x in [0,1] of NDC, y in [-0.5,0.5]
        float m4[16] = {0.5f, 0, 0, 0,  0, 0.5f, 0, 0,  0, 0, 1, 0,  0.5f, 0, 0, 1};
        glUniformMatrix4fv(glGetUniformLocation(mp, "m"), 1, 0, m4);
        glUniform4f(glGetUniformLocation(mp, "tint"), 1, 0, 1, 1);
        glClear(0x4000); glDrawArrays(5, 0, 4);
        CHECK_PIXEL("mat4 uniform, inside", 192, 128, 255, 0, 255);
        CHECK_PIXEL("mat4 uniform, outside", 64, 128, 0, 0, 0);
        // 12: the matrix changed between two draws in one frame
        float m5[16] = {0.5f, 0, 0, 0,  0, 0.5f, 0, 0,  0, 0, 1, 0,  -0.5f, 0, 0, 1};
        glUniformMatrix4fv(glGetUniformLocation(mp, "m"), 1, 0, m5);
        glUniform4f(glGetUniformLocation(mp, "tint"), 0, 1, 0, 1);
        glDrawArrays(5, 0, 4);
        CHECK_PIXEL("second matrix, same frame (left)", 64, 128, 0, 255, 0);
        CHECK_PIXEL("second matrix, first draw kept", 192, 128, 255, 0, 255);
        glDisableVertexAttribArray(1);
        // 13: 4x multisampled framebuffer resolved with glBlitFramebuffer
        GL(void, glRenderbufferStorageMultisample, unsigned, int, unsigned, int, int)
        GL(void, glBlitFramebuffer, int, int, int, int, int, int, int, int, unsigned, unsigned)
        unsigned mfb, mrb, mdb;
        glGenFramebuffers(1, &mfb); glBindFramebuffer(0x8D40, mfb);
        glGenRenderbuffers(1, &mrb); glBindRenderbuffer(0x8D41, mrb); glRenderbufferStorageMultisample(0x8D41, 4, 0x8058, W, H);
        glFramebufferRenderbuffer(0x8D40, 0x8CE0, 0x8D41, mrb);
        glGenRenderbuffers(1, &mdb); glBindRenderbuffer(0x8D41, mdb); glRenderbufferStorageMultisample(0x8D41, 4, 0x81A6, W, H);
        glFramebufferRenderbuffer(0x8D40, 0x8D00, 0x8D41, mdb);
        printf("%s 4x multisampled framebuffer\n", glCheckFramebufferStatus(0x8D40) == 0x8CD5 ? "ok  " : "FAIL");
        glEnable(0x0B71); glDepthFunc(0x0201); glClearColor(0, 0, 0, 1); glClear(0x4000 | 0x100);
        glUseProgram(pr); glBindBuffer(0x8892, vbo);
        glVertexAttribPointer(0, 3, 0x1406, 0, 0, 0);
        glBufferData(0x8892, sizeof nearq, nearq, 0x88E8); glUniform4f(uc, 0, 0, 1, 1); glDrawArrays(5, 0, 4);
        glBufferData(0x8892, sizeof farq, farq, 0x88E8); glUniform4f(uc, 1, 0, 0, 1); glDrawArrays(5, 0, 4);
        glDisable(0x0B71);
        glBindFramebuffer(0x8CA8 /* READ */, mfb); glBindFramebuffer(0x8CA9 /* DRAW */, fb);
        glBlitFramebuffer(0, 0, W, H, 0, 0, W, H, 0x4000, 0x2600);
        glBindFramebuffer(0x8D40, fb);
        CHECK_PIXEL("MSAA draw + depth, resolved", 128, 128, 0, 0, 255);
        printf("%s %d of %d feature checks passed\n", passed == total ? "ok  " : "FAIL", passed, total);
        green = passed == total ? W * H : 0;
    }
    if (tex) {
        // Texture upload: a 256x256 gradient drawn full-screen, then checked.
        GL(void, glGenTextures, int, unsigned *) GL(void, glBindTexture, unsigned, unsigned)
        GL(void, glTexImage2D, unsigned, int, int, int, int, int, unsigned, unsigned, const void *)
        GL(void, glTexParameteri, unsigned, unsigned, int)
        static uint8_t img[256 * 256 * 4];
        for (int y = 0; y < 256; y++)
            for (int x = 0; x < 256; x++) {
                uint8_t *p = img + (y * 256 + x) * 4;
                p[0] = x; p[1] = y; p[2] = 128; p[3] = 255;
            }
        unsigned tex;
        glGenTextures(1, &tex); glBindTexture(0x0DE1, tex);
        glTexParameteri(0x0DE1, 0x2801, 0x2600); glTexParameteri(0x0DE1, 0x2800, 0x2600);
        glTexImage2D(0x0DE1, 0, 0x1908, 256, 256, 0, 0x1908, 0x1401, img);
        const char *tvs = "attribute vec2 p; varying vec2 uv; void main() { uv = p * 0.5 + 0.5; gl_Position = vec4(p, 0.0, 1.0); }";
        const char *tfs = "precision mediump float; varying vec2 uv; uniform sampler2D t; void main() { gl_FragColor = texture2D(t, uv); }";
        unsigned tv = glCreateShader(0x8B31), tf = glCreateShader(0x8B30), tp = glCreateProgram();
        glShaderSource(tv, 1, &tvs, 0); glCompileShader(tv); glShaderSource(tf, 1, &tfs, 0); glCompileShader(tf);
        glAttachShader(tp, tv); glAttachShader(tp, tf); glBindAttribLocation(tp, 0, "p"); glLinkProgram(tp);
        glBindFramebuffer(0x8D40, fb);
        glViewport(0, 0, W, H);
        glUseProgram(tp);
        float quad[] = {-1, -1, 1, -1, -1, 1, 1, 1};
        glVertexAttribPointer(0, 2, 0x1406, 0, 0, quad); glEnableVertexAttribArray(0);
        glDrawArrays(5, 0, 4);
        glReadPixels(0, 0, W, H, 0x1908, 0x1401, px);
        int good = 0;
        for (int y = 0; y < H; y += 17)
            for (int x = 0; x < W; x += 13) {
                uint8_t *q = px + (y * W + x) * 4;
                good += abs(q[0] - x) <= 2 && abs(q[1] - y) <= 2 && abs(q[2] - 128) <= 2;
            }
        int total = ((H + 16) / 17) * ((W + 12) / 13);
        printf("%s texture upload: %d of %d samples right (row 0: %d,%d,%d  row 128: %d,%d,%d  row 255: %d,%d,%d)\n",
            good == total ? "ok  " : "FAIL", good, total,
            px[0], px[1], px[2], px[(128 * W) * 4], px[(128 * W) * 4 + 1], px[(128 * W) * 4 + 2], px[(255 * W) * 4], px[(255 * W) * 4 + 1], px[(255 * W) * 4 + 2]);
        green = good == total ? W * H : 0;
    }
    int ok = (hw || sw) && green == W * H && linked;
    printf("%s\n", ok ? "GLTEST PASSED" : "GLTEST FAILED");
    return !ok;
}
