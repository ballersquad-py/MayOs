/* drmtest: checks MayOS's vmwgfx DRM device the way Mesa's svga driver
 * probes it. Needs /etc/gpu3d (run: touch /etc/gpu3d, then reboot). */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
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

    printf("%s\n", fails ? "DRMTEST FAILED" : "DRMTEST PASSED");
    return fails != 0;
}
