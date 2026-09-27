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
