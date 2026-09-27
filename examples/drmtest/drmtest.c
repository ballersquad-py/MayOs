/* drmtest: checks MayOS's vmwgfx DRM device the way Mesa's svga driver
 * probes it. Needs /etc/gpu3d (run: touch /etc/gpu3d, then reboot). */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

struct drm_version {
    int major, minor, patch;
    size_t name_len; char *name;
    size_t date_len; char *date;
    size_t desc_len; char *desc;
};
struct drm_get_cap { uint64_t capability, value; };
struct vmw_getparam { uint64_t value; uint32_t param, pad; };
struct vmw_3dcap { uint64_t buffer; uint32_t max_size, pad; };

#define DRM_IOCTL_VERSION _IOWR('d', 0x00, struct drm_version)
#define DRM_IOCTL_GET_CAP _IOWR('d', 0x0c, struct drm_get_cap)
#define VMW_GET_PARAM _IOWR('d', 0x40, struct vmw_getparam)
#define VMW_GET_3D_CAP _IOW('d', 0x4d, struct vmw_3dcap)

static int fails;
#define CHECK(c, ...) do { printf((c) ? "ok   " : "FAIL "); printf(__VA_ARGS__); printf("\n"); if (!(c)) fails++; } while (0)

int main(void) {
    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    CHECK(fd >= 0, "open /dev/dri/renderD128 (needs /etc/gpu3d and the VMSVGA adapter with 3D)");
    if (fd < 0)
        return 1;
    struct stat st;
    CHECK(fstat(fd, &st) == 0 && S_ISCHR(st.st_mode) && major(st.st_rdev) == 226 && minor(st.st_rdev) == 128, "device number 226:128");

    char name[32] = {0}, date[32] = {0}, desc[128] = {0};
    struct drm_version v = {0};
    v.name = name; v.name_len = sizeof name - 1;
    v.date = date; v.date_len = sizeof date - 1;
    v.desc = desc; v.desc_len = sizeof desc - 1;
    CHECK(ioctl(fd, DRM_IOCTL_VERSION, &v) == 0 && !strcmp(name, "vmwgfx"), "driver %s %d.%d.%d (%s)", name, v.major, v.minor, v.patch, desc);

    struct drm_get_cap c = {5, 0};
    CHECK(ioctl(fd, DRM_IOCTL_GET_CAP, &c) == 0 && c.value == 3, "PRIME cap %llu", (unsigned long long)c.value);

    static const char *names[] = {"streams", "free streams", "3D", "hw caps", "fifo caps", "max fb", "fifo hw version",
        "max surface mem", "3D caps size", "max MOB mem", "max MOB size", "screen target", "DX", "hw caps2",
        "SM4.1", "SM5", "GL4.3", "device id"};
    uint64_t p[18] = {0};
    for (int i = 0; i < 18; i++) {
        struct vmw_getparam g = {0, (uint32_t)i, 0};
        int r = ioctl(fd, VMW_GET_PARAM, &g);
        p[i] = g.value;
        printf("%s   param %-16s = %#llx\n", r == 0 ? "ok " : "FAIL", names[i], (unsigned long long)g.value);
        if (r) fails++;
    }
    CHECK(p[2] == 1, "3D available");
    CHECK(p[12] == 1, "DX (shader model 4) context available");

    static uint32_t caps[1024];
    struct vmw_3dcap q = {(uint64_t)(uintptr_t)caps, (uint32_t)p[8], 0};
    int nz = 0;
    int r = ioctl(fd, VMW_GET_3D_CAP, &q);
    for (unsigned i = 0; i < p[8] / 4 && i < 1024; i++)
        nz += caps[i] != 0;
    CHECK(r == 0 && nz > 50, "3D caps: %llu bytes, %d set", (unsigned long long)p[8], nz);

    // ---- stage 2: objects and command submission, as Mesa uses them ----
    union { struct { uint32_t size, pad; } req; struct { uint64_t map_handle; uint32_t handle, gmr_id, gmr_off, pad; } rep; } bo;
    const uint32_t W = 64, H = 64;
    memset(&bo, 0, sizeof bo);
    bo.req.size = W * H * 4;
    int r2 = ioctl(fd, _IOWR('d', 0x41, bo), &bo);
    CHECK(r2 == 0, "allocate a GPU buffer (handle %u)", bo.rep.handle);
    uint32_t bo_handle = bo.rep.handle;
    uint32_t *px = mmap(0, W * H * 4, PROT_READ | PROT_WRITE, MAP_SHARED, fd, bo.rep.map_handle);
    CHECK(px != MAP_FAILED, "map the buffer");
    if (px == MAP_FAILED || r2) goto done;
    for (uint32_t i = 0; i < W * H; i++) px[i] = 0x11223344;

    uint32_t sreq[22] = {0};
    sreq[0] = (1u << 24) | (1u << 23);   // bind render target | shader resource
    sreq[1] = 68;                        // R8G8B8A8_UNORM
    sreq[2] = 1;                         // mip levels
    sreq[3] = 0;                         // drm flags
    sreq[6] = bo_handle;                 // backing buffer
    sreq[7] = 0;                         // array size
    sreq[8] = W; sreq[9] = H; sreq[10] = 1;
    sreq[11] = 4;                        // surface version
    r2 = ioctl(fd, _IOWR('d', 0x5b, sreq), sreq);
    uint32_t sid = sreq[0];
    CHECK(r2 == 0, "create a DX surface on the buffer (sid %u)", sid);

    union { uint32_t req; struct { int32_t cid; uint32_t pad; } rep; } ctx = {1};
    r2 = ioctl(fd, _IOWR('d', 0x5a, ctx), &ctx);
    uint32_t cid = ctx.rep.cid;
    CHECK(r2 == 0, "create a DX context (cid %u)", cid);

    // Commands: define a render-target view, clear it, read it back.
    uint32_t cmd[64]; int n = 0;
    float rgba[4] = {1.0f, 0.5f, 0.25f, 1.0f};
    cmd[n++] = 1187; cmd[n++] = 7 * 4;             // DX_DEFINE_RENDERTARGET_VIEW
    cmd[n++] = 0; cmd[n++] = sid; cmd[n++] = 68; cmd[n++] = 3; cmd[n++] = 0; cmd[n++] = 0; cmd[n++] = 1;
    cmd[n++] = 1176; cmd[n++] = 5 * 4;             // DX_CLEAR_RENDERTARGET_VIEW
    cmd[n++] = 0; memcpy(&cmd[n], rgba, 16); n += 4;
    cmd[n++] = 1183; cmd[n++] = 2 * 4;             // DX_READBACK_SUBRESOURCE
    cmd[n++] = sid; cmd[n++] = 0;
    uint32_t fence[6] = {0};
    struct { uint64_t commands; uint32_t size, throttle; uint64_t fence_rep; uint32_t version, flags, context, imported; } eb =
        {(uint64_t)(uintptr_t)cmd, (uint32_t)(n * 4), 0, (uint64_t)(uintptr_t)fence, 2, 0, cid, -1};
    r2 = ioctl(fd, _IOW('d', 0x4c, eb), &eb);
    CHECK(r2 == 0 && fence[5] == 0, "GPU ran a DX clear (fence %u)", fence[0]);
    int good = 0;
    for (uint32_t i = 0; i < W * H; i++) {
        uint32_t v = px[i], r = v & 0xff, g = (v >> 8) & 0xff, b = (v >> 16) & 0xff, a = v >> 24;
        good += r == 0xff && (g == 0x7f || g == 0x80) && (b == 0x3f || b == 0x40) && a == 0xff;
    }
    CHECK(good == (int)(W * H), "cleared pixels read back: %d of %u are (255,128,64,255), first %#x", good, W * H, px[0]);
done:
    printf("%s\n", fails ? "DRMTEST FAILED" : "DRMTEST PASSED");
    return fails != 0;
}
