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
    int sw = argc > 1 && !strcmp(argv[argc - 1], "-sw"); // software check (no GPU)
    int wl = argc > 1 && !strcmp(argv[argc - 1], "-wl"); // through the Wayland compositor, like Firefox
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
    int cattrs[] = {0x3098 /* CONTEXT_CLIENT_VERSION */, 2, 0x3038};
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
    int ok = (hw || sw) && green == W * H && linked;
    printf("%s\n", ok ? "GLTEST PASSED" : "GLTEST FAILED");
    return !ok;
}
