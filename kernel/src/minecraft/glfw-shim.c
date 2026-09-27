/* libglfw-mayos.so: Alpine's GLFW with glfwSetWindowIcon as a no-op.
 * On Wayland GLFW reports setting the icon as an error, which older
 * Minecraft versions treat as fatal. LWJGL looks functions up in this
 * library with dlsym, which falls through to libglfw.so.3 (DT_NEEDED)
 * for everything else.
 * Build: gcc -shared -fPIC -nostdlib -O2 -Wl,--no-as-needed -o libglfw-mayos.so glfw-shim.c -L<dir with libglfw.so.3> -l:libglfw.so.3
 */
void glfwSetWindowIcon(void *window, int count, const void *images) {
    (void)window; (void)count; (void)images;
}

/* GLFW 3.5 text input (IME) API, which LWJGL 3.4 (legacy-lwjgl3 for
 * Minecraft 1.8.9) calls unconditionally; Alpine's GLFW 3.4 lacks it.
 * No IME on MayOS: callbacks are never called, queries return nothing. */
void *glfwSetPreeditCallback(void *window, void *cb) { (void)window; (void)cb; return 0; }
void *glfwSetIMEStatusCallback(void *window, void *cb) { (void)window; (void)cb; return 0; }
void *glfwSetPreeditCandidateCallback(void *window, void *cb) { (void)window; (void)cb; return 0; }
void glfwGetPreeditCursorRectangle(void *window, int *x, int *y, int *w, int *h) {
    (void)window;
    if (x) *x = 0;
    if (y) *y = 0;
    if (w) *w = 0;
    if (h) *h = 0;
}
void glfwSetPreeditCursorRectangle(void *window, int x, int y, int w, int h) { (void)window; (void)x; (void)y; (void)w; (void)h; }
void glfwResetPreeditText(void *window) { (void)window; }
unsigned int *glfwGetPreeditCandidate(void *window, int index, int *length) { (void)window; (void)index; if (length) *length = 0; return 0; }
