// MayOS's Minecraft launcher: downloads Minecraft: Java Edition from
// Mojang's servers and starts it in offline mode (singleplayer).
//
//   minecraft [version] [--user NAME] [--memory 4G] [--mods | --vanilla | --forge | --ornithe] [--software] [--size WxH] [--fullscreen] [--dry-run] [--debug]
//
// Needs only a JDK (runs as a single source file). Files go to
// $HOME/.minecraft, like the official launcher.

import java.io.*;
import java.net.URI;
import java.net.http.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;

public class Launcher {
    static final String BUILD = "2026-10-01";
    static final String MANIFEST = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
    static HttpClient HTTP;
    static final String CA_BUNDLE = "/etc/ssl/certs/ca-certificates.crt";

    // Alpine makes Java's cacerts in a package trigger that MayOS does not
    // run, so Java would trust nobody: build a trust store from the
    // system's PEM bundle and use it here and in the game.
    static Path trustStore(Path mc) throws Exception {
        Path pem = Paths.get(CA_BUNDLE);
        if (!Files.exists(pem)) return null;
        Path store = mc.resolve("cacerts.p12");
        if (!Files.exists(store) || Files.getLastModifiedTime(store).compareTo(Files.getLastModifiedTime(pem)) < 0) {
            java.security.KeyStore ks = java.security.KeyStore.getInstance("PKCS12");
            ks.load(null, null);
            int n = 0;
            try (InputStream in = Files.newInputStream(pem)) {
                for (java.security.cert.Certificate c : java.security.cert.CertificateFactory.getInstance("X.509").generateCertificates(in))
                    ks.setCertificateEntry("ca" + n++, c);
            }
            Files.createDirectories(mc);
            try (OutputStream out = Files.newOutputStream(store)) { ks.store(out, "changeit".toCharArray()); }
        }
        System.setProperty("javax.net.ssl.trustStore", store.toString());
        System.setProperty("javax.net.ssl.trustStoreType", "PKCS12");
        System.setProperty("javax.net.ssl.trustStorePassword", "changeit");
        return store;
    }

    public static void main(String[] args) throws Exception {
        String version = null, user = "Player";
        boolean dry = false, debug = false;
        boolean login = false, logout = false;
        boolean art = false;
        boolean packOnly = false, forge = false, ornithe = false, software = false, forceGpu = false;
        System.out.println("MayOS Minecraft launcher, build " + BUILD);
        Boolean mods = null; // --mods / --vanilla; default: mods only where needed (1.8.9 & co)
        // Java heap: --memory 4G, or $MC_MEMORY, or /etc/minecraft-memory.
        String memory = System.getenv("MC_MEMORY");
        try {
            if (memory == null) memory = Files.readString(Paths.get("/etc/minecraft-memory")).trim();
        } catch (IOException e) {
            // not set
        }
        for (int i = 0; i < args.length; i++) {
            if (args[i].equals("--user") && i + 1 < args.length) user = args[++i];
            else if (args[i].equals("--dry-run")) dry = true;
            else if (args[i].equals("--debug")) debug = true;
            else if (args[i].equals("--memory") && i + 1 < args.length) memory = args[++i];
            else if (args[i].equals("--mods")) mods = true;
            else if (args[i].equals("--pack-only")) { mods = true; packOnly = true; }
            else if (args[i].equals("--vanilla")) mods = false;
            else if (args[i].equals("--forge")) { mods = true; forge = true; }
            else if (args[i].equals("--ornithe")) { mods = true; ornithe = true; }
            else if (args[i].equals("--software")) software = true;
            else if (args[i].equals("--gpu")) forceGpu = true;
            else if (args[i].equals("--size") && i + 1 < args.length) x11Size = args[++i];
            else if (args[i].equals("--fullscreen")) x11Fullscreen = true;
            else if (args[i].equals("--login")) login = true;
            else if (args[i].equals("--logout")) logout = true;
            else if (args[i].equals("--art")) art = true;
            else version = args[i];
        }
        Path home = Paths.get(System.getProperty("user.home", "/home"));
        if (!Files.isWritable(home)) home = Paths.get("/home");
        Path mc = home.resolve(".minecraft");
        Path store = trustStore(mc);
        HTTP = HttpClient.newBuilder().followRedirects(HttpClient.Redirect.NORMAL).build();
        cacheDir = mc.resolve("cache");
        Path accountFile = mc.resolve("mayos-account.json");
        if (art) {
            extractArt(mc, null);
            return;
        }
        if (logout) {
            Files.deleteIfExists(accountFile);
            System.out.println("Signed out: playing offline.");
            return;
        }
        if (login) {
            try {
                Account.login(accountFile);
            } catch (Exception e) {
                System.out.println("Microsoft sign-in: " + e.getMessage());
                System.exit(1);
            }
            return;
        }
        // No --user given: /etc/minecraft-user (set by the MayOS launcher app).
        if (user.equals("Player")) {
            try {
                String u = Files.readString(Paths.get("/etc/minecraft-user")).trim();
                if (u.matches("[A-Za-z0-9_]{3,16}")) user = u;
            } catch (IOException e) {
                // default name
            }
        }
        Account account = Files.exists(accountFile) ? Account.load(accountFile) : null;
        if (account != null) {
            user = account.name;
            System.out.println("Account: " + account.name + " (Microsoft)");
        }

        Map<String, Object> manifest = obj(Json.parse(fetchString(MANIFEST)));
        String versionUrl = null;
        if (version == null) {
            // The newest release this Java can run (Alpine 3.22 has Java 21).
            for (Object o : list(manifest.get("versions"))) {
                Map<String, Object> m = obj(o);
                if (!"release".equals(m.get("type"))) continue;
                Map<String, Object> jv = obj(obj(Json.parse(fetchString(str(m.get("url"))))).get("javaVersion"));
                if (jv == null || num(jv.get("majorVersion")) <= Runtime.version().feature()) {
                    version = str(m.get("id"));
                    versionUrl = str(m.get("url"));
                    break;
                }
            }
        }
        for (Object v : list(manifest.get("versions")))
            if (str(obj(v).get("id")).equals(version)) versionUrl = str(obj(v).get("url"));
        if (versionUrl == null) throw new RuntimeException("no Minecraft version " + version);
        System.out.println("Minecraft " + version);

        Path vdir = mc.resolve("versions").resolve(version);
        Path vjson = vdir.resolve(version + ".json");
        download(versionUrl, vjson, -1);
        Map<String, Object> v = obj(Json.parse(Files.readString(vjson)));
        Map<String, Object> jv = obj(v.get("javaVersion"));
        if (jv != null && num(jv.get("majorVersion")) > Runtime.version().feature())
            System.out.println("Warning: Minecraft " + version + " wants Java " + num(jv.get("majorVersion")) + ", this is Java " + Runtime.version().feature());

        // Versions before 1.13 use LWJGL 2, which needs X11: they run with
        // the Ornithe loader and the legacy-lwjgl3 mod (LWJGL 3 on Wayland).
        boolean legacy = v.get("arguments") == null;
        if (mods == null) mods = legacy;
        // 1.8.9 with mods: Forge + OptiFine on X11 (Java 8) when installed.
        if (legacy && mods && !forge && !ornithe && version.equals("1.8.9")
                && (Files.exists(Paths.get("/usr/lib/jvm/java-1.8-openjdk/bin/java")) || Files.exists(Paths.get("/usr/lib/jvm/java-1.8-openjdk/jre/bin/java")))) {
            forge = true;
            System.out.println("Using Forge + OptiFine (--ornithe for the Ornithe/Fabric pack)");
        }
        if (legacy && !mods) System.out.println("Warning: " + version + " needs --mods (LWJGL 3) to open a window on MayOS");
        if (forge && !legacy) throw new RuntimeException("--forge is for versions before 1.13 (it runs them on X11 through Xwayland); use --mods for Fabric");
        Map<String, Object> loader = !mods ? null : forge ? forgeProfile(mc, version) : loaderProfile(version, legacy);
        String loaderName = forge ? "forge" : legacy ? "ornithe" : "fabric";
        Path gameDir = mods ? mc.resolve("instances").resolve(version + "-" + loaderName) : mc;
        if (mods) System.out.println("Mod loader: " + str(loader.get("id")) + "; mods in " + gameDir.resolve("mods"));
        if (packOnly) {
            installPack(gameDir.resolve("mods"), version, legacy, forge);
            return;
        }

        // Client jar and libraries.
        List<String[]> jobs = new ArrayList<>(); // url, path, size
        Path client = vdir.resolve(version + ".jar");
        Map<String, Object> cd = obj(obj(v.get("downloads")).get("client"));
        jobs.add(new String[] {str(cd.get("url")), client.toString(), String.valueOf(num(cd.get("size")))});
        List<String> cp = new ArrayList<>();
        Set<String> loaderArtifacts = new HashSet<>();
        List<Path> nativeJars = new ArrayList<>();
        if (loader != null) {
            for (Object lo : list(loader.get("libraries"))) {
                Map<String, Object> lib = obj(lo);
                if (Boolean.FALSE.equals(lib.get("clientreq"))) continue; // server only
                String[] n = str(lib.get("name")).split(":");
                String path = n[0].replace('.', '/') + "/" + n[1] + "/" + n[2] + "/" + n[1] + "-" + n[2] + ".jar";
                String base = lib.get("url") == null ? "https://libraries.minecraft.net/" : str(lib.get("url"));
                if (!base.endsWith("/")) base += "/";
                Path p = mc.resolve("libraries").resolve(path);
                jobs.add(new String[] {base + path, p.toString(), String.valueOf(num(lib.get("size")))});
                cp.add(p.toString());
                loaderArtifacts.add(n[0] + ":" + n[1]);
            }
        }
        if (legacy && mods && !forge) {
            // Libraries legacy-lwjgl3 and Ornithe's standard libraries use that old
            // versions do not ship (SLF4J, fastutil, a newer log4j).
            for (String path : new String[] {"org/slf4j/slf4j-api/2.0.16/slf4j-api-2.0.16.jar", "org/slf4j/slf4j-simple/2.0.16/slf4j-simple-2.0.16.jar", "it/unimi/dsi/fastutil/8.5.15/fastutil-8.5.15.jar",
                    "org/apache/logging/log4j/log4j-api/2.19.0/log4j-api-2.19.0.jar", "org/apache/logging/log4j/log4j-core/2.19.0/log4j-core-2.19.0.jar"}) {
                Path p = mc.resolve("libraries").resolve(path);
                jobs.add(new String[] {"https://repo1.maven.org/maven2/" + path, p.toString(), "-1"});
                cp.add(p.toString());
            }
        }
        for (Object lo : list(v.get("libraries"))) {
            Map<String, Object> lib = obj(lo);
            if (!allowed(lib.get("rules"))) continue;
            String[] n = String.valueOf(lib.get("name")).split(":");
            if (n.length > 1 && loaderArtifacts.contains(n[0] + ":" + n[1])) continue; // the loader's newer copy
            if (legacy && mods && !forge && n[0].equals("org.lwjgl.lwjgl")) continue; // LWJGL 2: replaced by legacy-lwjgl3
            if (legacy && mods && !forge && n[0].equals("org.apache.logging.log4j")) continue; // 2.0-beta9: replaced by 2.19 below
            // Native libraries (LWJGL 2, jinput): unpacked below.
            Map<String, Object> nat = obj(lib.get("natives"));
            if (nat != null && nat.get("linux") != null) {
                Map<String, Object> dlc = obj(lib.get("downloads"));
                Map<String, Object> cls = dlc == null ? null : obj(dlc.get("classifiers"));
                Map<String, Object> na = cls == null ? null : obj(cls.get(str(nat.get("linux")).replace("${arch}", "64")));
                if (na != null) {
                    Path p = mc.resolve("libraries").resolve(str(na.get("path")));
                    jobs.add(new String[] {str(na.get("url")), p.toString(), String.valueOf(num(na.get("size")))});
                    nativeJars.add(p);
                }
            }
            Map<String, Object> dl = obj(lib.get("downloads"));
            if (dl == null || dl.get("artifact") == null) continue;
            Map<String, Object> a = obj(dl.get("artifact"));
            Path p = mc.resolve("libraries").resolve(str(a.get("path")));
            jobs.add(new String[] {str(a.get("url")), p.toString(), String.valueOf(num(a.get("size")))});
            cp.add(p.toString());
        }
        cp.add(client.toString());

        // Assets (sounds, languages, textures outside the jar).
        Map<String, Object> ai = obj(v.get("assetIndex"));
        String assetsId = str(ai.get("id"));
        Path assets = mc.resolve("assets");
        Path indexFile = assets.resolve("indexes").resolve(assetsId + ".json");
        download(str(ai.get("url")), indexFile, num(ai.get("size")));
        if (!dry && System.getenv("MC_SKIP_ASSETS") == null) {
            for (Map.Entry<String, Object> e : obj(obj(Json.parse(Files.readString(indexFile))).get("objects")).entrySet()) {
                String hash = str(obj(e.getValue()).get("hash"));
                String sub = hash.substring(0, 2) + "/" + hash;
                jobs.add(new String[] {"https://resources.download.minecraft.net/" + sub, assets.resolve("objects").resolve(sub).toString(),
                        String.valueOf(num(obj(e.getValue()).get("size")))});
            }
        }
        downloadAll(dry ? jobs.subList(0, 0) : jobs);

        // Command line.
        Path natives = vdir.resolve("natives");
        Files.createDirectories(natives);
        for (Path jar : nativeJars) {
            if (!Files.exists(jar)) continue;
            try (java.util.zip.ZipInputStream z = new java.util.zip.ZipInputStream(Files.newInputStream(jar))) {
                for (java.util.zip.ZipEntry e; (e = z.getNextEntry()) != null; ) {
                    String name = e.getName();
                    if (e.isDirectory() || name.startsWith("META-INF") || name.contains("/")) continue;
                    Files.copy(z, natives.resolve(name), StandardCopyOption.REPLACE_EXISTING);
                }
            }
        }
        Map<String, String> vars = new HashMap<>();
        vars.put("auth_player_name", user);
        vars.put("version_name", version);
        vars.put("game_directory", gameDir.toString());
        if (mods && !dry) installPack(gameDir.resolve("mods"), version, legacy, forge);
        // Fast defaults for a fresh game directory (software GL is slow):
        // the player's own options.txt is never touched.
        Path opts = gameDir.resolve("options.txt");
        if (!dry && !Files.exists(opts)) {
            Files.createDirectories(gameDir);
            String common = "renderDistance:6\nparticles:2\nmaxFps:260\nenableVsync:false\nentityShadows:false\nrenderClouds:false\nmipmapLevels:0\n";
            Files.writeString(opts, common + (legacy || forge
                    ? "fancyGraphics:false\nao:0\npauseOnLostFocus:false\n"
                    : "graphicsMode:0\nao:false\nsimulationDistance:5\nbiomeBlendRadius:0\nrenderClouds:\"false\"\nonboardAccessibility:false\nskipMultiplayerWarning:true\ntutorialStep:none\n"));
            if (forge) Files.writeString(gameDir.resolve("optionsof.txt"),
                    "ofFastRender:false\nofFastMath:true\nofSmoothFps:false\nofChunkUpdates:2\nofChunkUpdatesDynamic:true\nofAaLevel:0\nofAfLevel:1\nofClouds:3\nofTrees:1\nofDroppedItems:1\nofRainSplash:false\nofAnimatedWater:1\nofAnimatedLava:1\nofVignette:1\nofSky:true\nofDynamicFov:false\n");
        }
        if (forge && !dry) {
            // OptiFine's Fast Render leaves the world undrawn on llvmpipe.
            Path of = gameDir.resolve("optionsof.txt");
            if (Files.exists(of)) {
                String t = Files.readString(of);
                if (t.contains("ofFastRender:true")) Files.writeString(of, t.replace("ofFastRender:true", "ofFastRender:false"));
            }
            // X11 focus comes and goes without a window manager; losing it
            // would reopen the pause menu at once.
            Path op = gameDir.resolve("options.txt");
            if (Files.exists(op)) {
                String t = Files.readString(op);
                if (!t.contains("pauseOnLostFocus:false"))
                    Files.writeString(op, t.replace("pauseOnLostFocus:true\n", "") + (t.endsWith("\n") || t.isEmpty() ? "" : "\n") + "pauseOnLostFocus:false\n");
            }
        }
        if (forge) {
            // Forge's loading splash draws from a second thread with a shared
            // GL context, which crashes Mesa's llvmpipe.
            Path splash = gameDir.resolve("config").resolve("splash.properties");
            Files.createDirectories(splash.getParent());
            String sp = Files.exists(splash) ? Files.readString(splash) : "";
            if (!sp.contains("enabled=false")) Files.writeString(splash, sp.replace("enabled=true", "") + "\nenabled=false\n");
        }
        vars.put("assets_root", assets.toString());
        vars.put("game_assets", assets.toString());
        vars.put("assets_index_name", assetsId);
        vars.put("auth_uuid", UUID.nameUUIDFromBytes(("OfflinePlayer:" + user).getBytes()).toString().replace("-", ""));
        vars.put("auth_access_token", "0");
        if (account != null) {
            vars.put("auth_uuid", account.uuid);
            vars.put("auth_access_token", account.token);
            vars.put("auth_session", account.token);
            vars.put("user_type", "msa");
        }
        vars.put("auth_session", "0");
        vars.put("clientid", "0");
        vars.put("auth_xuid", "0");
        vars.putIfAbsent("user_type", "legacy");
        vars.put("user_properties", "{}");
        vars.put("version_type", str(v.get("type")));
        vars.put("natives_directory", natives.toString());
        vars.put("launcher_name", "MayOS");
        vars.put("launcher_version", "1");
        vars.put("classpath", String.join(File.pathSeparator, cp));
        vars.put("classpath_separator", File.pathSeparator);
        vars.put("library_directory", mc.resolve("libraries").toString());

        String java = ProcessHandle.current().info().command().orElse("java");
        // Forge 1.8.9 (LaunchWrapper) needs Java 8.
        if (forge) {
            java = "/usr/lib/jvm/java-1.8-openjdk/bin/java";
            if (!Files.exists(Paths.get(java))) java = "/usr/lib/jvm/java-1.8-openjdk/jre/bin/java";
            if (!Files.exists(Paths.get(java))) throw new RuntimeException("Java 8 is missing: run pkg install minecraft");
            // Which Java really runs (Forge 1.8.9 fails on anything newer than 8).
            ProcessBuilder vb = new ProcessBuilder(java, "-version").redirectErrorStream(true);
            // Alpine's Java 21 launcher exports LD_LIBRARY_PATH to its own lib dir;
            // Java 8 would then load Java 21's libjli/libjvm.
            vb.environment().remove("LD_LIBRARY_PATH");
            Process v8 = vb.start();
            String vtext = new String(v8.getInputStream().readAllBytes()).trim();
            v8.waitFor();
            System.out.println("Java for Forge: " + java + ": " + vtext.replace('\n', ' '));
        }
        List<String> cmd = new ArrayList<>();
        cmd.add(java);
        if (store != null) {
            cmd.add("-Djavax.net.ssl.trustStore=" + store);
            cmd.add("-Djavax.net.ssl.trustStoreType=PKCS12");
            cmd.add("-Djavax.net.ssl.trustStorePassword=changeit");
        }
        // LWJGL's bundled jemalloc is a glibc build that crashes in its
        // init under musl: use the C library's malloc.
        cmd.add("-Dorg.lwjgl.system.allocator=system");
        if (memory == null || memory.isEmpty()) memory = "2G";
        if (memory.matches("\\d+")) memory += "M";
        System.out.println("Memory: " + memory + " (change with --memory 4G or echo 4G > /etc/minecraft-memory)");
        cmd.add("-Xms" + (memory.matches("\\d+[gG]") && Integer.parseInt(memory.replaceAll("\\D", "")) > 1 ? "1G" : "256M"));
        cmd.add("-Xmx" + memory);
        // (libglfw-mayos.so: Alpine's GLFW without the window icon call,
        // which fails on Wayland and stops older versions.)
        // Use Alpine's (musl) GLFW, OpenAL and Mesa instead of the glibc
        // builds inside Mojang's LWJGL jars.
        for (String[] l : new String[][] {{"glfw", Files.exists(Paths.get("/usr/share/minecraft/libglfw-mayos.so")) ? "/usr/share/minecraft/libglfw-mayos.so" : "/usr/lib/libglfw.so.3"}, {"openal", "/usr/lib/libopenal.so.1"}, {"opengl", "/usr/lib/libGL.so.1"}})
            if (Files.exists(Paths.get(l[1]))) cmd.add("-Dorg.lwjgl." + l[0] + ".libname=" + l[1]);
        Map<String, Object> arguments = obj(v.get("arguments"));
        if (arguments != null) {
            addArgs(cmd, arguments.get("jvm"), vars);
            if (loader != null) addArgs(cmd, obj(loader.get("arguments")).get("jvm"), vars);
            cmd.add(str((loader != null ? loader : v).get("mainClass")));
            addArgs(cmd, arguments.get("game"), vars);
            if (loader != null) addArgs(cmd, obj(loader.get("arguments")).get("game"), vars);
        } else {
            // Versions before 1.13.
            cmd.add("-Djava.library.path=" + natives);
            cmd.add("-cp");
            cmd.add(vars.get("classpath"));
            if (loader != null && loader.get("arguments") != null) addArgs(cmd, obj(loader.get("arguments")).get("jvm"), vars);
            cmd.add(str((loader != null ? loader : v).get("mainClass")));
            Map<String, Object> argSrc = loader != null && loader.get("minecraftArguments") != null ? loader : v;
            for (String a : str(argSrc.get("minecraftArguments")).split(" ")) cmd.add(subst(a, vars));
            if (forge) {
                // No window manager under Xwayland: open the game at the X screen's size.
                String[] wh = x11Geometry().split("x");
                Collections.addAll(cmd, "--width", wh[0], "--height", wh[1]);
            }
        }
        if (dry) {
            System.out.println(String.join(" ", cmd));
            return;
        }
        extractArt(mc, client);
        System.out.println("Starting Minecraft...");
        Files.createDirectories(gameDir);
        ProcessBuilder pb = new ProcessBuilder(cmd).directory(gameDir.toFile()).inheritIO();
        // legacy-lwjgl3: GLFW (on MayOS's Wayland compositor), not SDL.
        if (legacy && mods) pb.environment().put("LEGACY_LWJGL3_USE_SDL", "false");
        pb.environment().put("XDG_SESSION_TYPE", "wayland");
        pb.environment().remove("LD_LIBRARY_PATH");
        pb.environment().remove("DISPLAY");
        // Cursor theme for GLFW/Xwayland (adwaita-icon-theme).
        pb.environment().putIfAbsent("XCURSOR_THEME", "Adwaita");
        pb.environment().putIfAbsent("XCURSOR_PATH", "/usr/share/icons");
        // No GPU device: Mesa's software renderer (llvmpipe) drawing into
        // wl_shm buffers. Otherwise Mesa may try zink/Vulkan, which is not
        // installed, and never produce a window.
        int cpus = Runtime.getRuntime().availableProcessors();
        boolean gpu = Files.exists(Paths.get("/dev/dri/renderD128")) && !"0".equals(System.getenv("MC_GPU")) && !software;
        // 1.21.5+ (new renderer) draws black on VirtualBox's GPU: software
        // unless --gpu.
        if (gpu && !forceGpu && String.valueOf(v.get("releaseTime")).compareTo("2025-03-25") >= 0) {
            gpu = false;
            System.out.println("Note: " + version + " shows a black screen on VirtualBox's GPU; using software (try --gpu to test)");
        }
        System.out.println("CPUs: " + cpus + (cpus == 1 ? " (give the VM more cores for more speed)" : ""));
        System.out.println(gpu ? "Graphics: GPU (VMware SVGA 3D through Mesa's svga driver)"
                : "Graphics: software (llvmpipe, " + cpus + " threads). For the GPU: VirtualBox display VMSVGA with\n"
                + "  'Enable 3D Acceleration' on, then 'touch /etc/gpu3d' in MayOS and reboot.");
        if (forge) {
            // X11 (Xwayland) has no GPU sharing on MayOS yet: software GL.
            gpu = false;
            System.out.println("Graphics under X11: software (llvmpipe, " + cpus + " threads)");
        }
        if (gpu) {
            // MayOS starts Linux programs on software OpenGL unless
            // Firefox's GPU mode is on: undo that for the game.
            pb.environment().remove("LIBGL_ALWAYS_SOFTWARE");
            pb.environment().remove("GALLIUM_DRIVER");
        } else {
            pb.environment().putIfAbsent("LIBGL_ALWAYS_SOFTWARE", "1");
            pb.environment().putIfAbsent("GALLIUM_DRIVER", "llvmpipe");
        }
        if (debug) {
            pb.environment().put("EGL_LOG_LEVEL", "debug");
            pb.environment().put("LIBGL_DEBUG", "verbose");
            pb.environment().put("MESA_DEBUG", "1");
        }
        // LWJGL's natives are glibc builds. musl takes libc.so.6 and friends
        // to mean itself, so gcompat's glibc symbols (__snprintf_chk, ...)
        // are only there when preloaded; unresolved ones jump to nowhere.
        if (Files.exists(Paths.get("/lib/libgcompat.so.0"))) pb.environment().merge("LD_PRELOAD", "/lib/libgcompat.so.0", (a, b) -> b + ":" + a);
        Process xserver = null;
        if (forge) {
            // LWJGL 2 needs X11: a rootful Xwayland is an X server in a MayOS window.
            xserver = startXwayland(pb);
        }
        if (forge) noPauseOnFocusLoss(gameDir);
        Process game = pb.start();
        if (forge) x11Helper(game, pb.environment().get("DISPLAY"));
        int code = game.waitFor();
        if (xserver != null) xserver.destroy();
        System.exit(code);
    }

    // Fabric (1.14+) or Ornithe (older versions) launcher profile.
    static Map<String, Object> loaderProfile(String version, boolean legacy) throws Exception {
        String meta = legacy ? "https://meta.ornithemc.net/v3/versions/fabric-loader/" : "https://meta.fabricmc.net/v2/versions/loader/";
        List<Object> loaders = list(Json.parse(fetchString(meta + version)));
        if (loaders.isEmpty()) throw new RuntimeException("no " + (legacy ? "Ornithe" : "Fabric") + " loader for Minecraft " + version);
        String lv = str(obj(obj(loaders.get(0)).get("loader")).get("version"));
        return obj(Json.parse(fetchString(meta + version + "/" + lv + "/profile/json")));
    }

    // Forge for old versions: the installer carries the version profile and
    // the Forge jar itself (no need to run it).
    static Map<String, Object> forgeProfile(Path mc, String version) throws Exception {
        String meta = fetchString("https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml");
        String best = null;
        java.util.regex.Matcher m = java.util.regex.Pattern.compile("<version>(" + java.util.regex.Pattern.quote(version) + "-[^<]+)</version>").matcher(meta);
        while (m.find()) if (best == null || newer(m.group(1), best)) best = m.group(1);
        if (best == null) throw new RuntimeException("no Forge for Minecraft " + version);
        Path inst = mc.resolve("libraries/net/minecraftforge/forge/" + best + "/forge-" + best + "-installer.jar");
        download("https://maven.minecraftforge.net/net/minecraftforge/forge/" + best + "/forge-" + best + "-installer.jar", inst, -1);
        Map<String, Object> profile;
        try (java.util.zip.ZipFile z = new java.util.zip.ZipFile(inst.toFile())) {
            profile = obj(Json.parse(new String(z.getInputStream(z.getEntry("install_profile.json")).readAllBytes(), "UTF-8")));
            Map<String, Object> install = obj(profile.get("install"));
            String[] n = str(install.get("path")).split(":");
            Path jar = mc.resolve("libraries").resolve(n[0].replace('.', '/') + "/" + n[1] + "/" + n[2] + "/" + n[1] + "-" + n[2] + ".jar");
            if (!Files.exists(jar)) {
                Files.createDirectories(jar.getParent());
                Files.copy(z.getInputStream(z.getEntry(str(install.get("filePath")))), jar, StandardCopyOption.REPLACE_EXISTING);
            }
        }
        Map<String, Object> vi = obj(profile.get("versionInfo"));
        vi.put("id", str(vi.get("id")));
        return vi;
    }

    // Compare version strings by their numbers.
    static boolean newer(String a, String b) {
        String[] x = a.split("\\D+"), y = b.split("\\D+");
        for (int i = 0; i < Math.min(x.length, y.length); i++) {
            if (x[i].isEmpty() || y[i].isEmpty()) continue;
            long p = Long.parseLong(x[i]), q = Long.parseLong(y[i]);
            if (p != q) return p > q;
        }
        return x.length > y.length;
    }

    static String x11Size = null;
    static boolean x11Fullscreen = false;

    // Xwayland has no window manager: nobody gives the game keyboard focus
    // (LWJGL 2 then thinks it is in the background) and nobody resizes it
    // when the X screen changes size with its MayOS window. This does a
    // window manager's two jobs, speaking X11 directly (no extra programs).
    static void x11Helper(Process game, String display) {
        Thread t = new Thread(() -> {
            int d = Integer.parseInt(display.replace(":", "").split("\\.")[0]);
            X11 x = null;
            long win = 0;
            int[] size = {0, 0};
            while (game.isAlive()) {
                try {
                    Thread.sleep(win == 0 ? 400 : 500);
                    if (x == null) x = X11.connect(d);
                    int[] root = x.geometry(x.root);
                    // The game's window: the biggest top-level window.
                    long best = 0;
                    int area = 0;
                    for (long c : x.children(x.root)) {
                        int[] g = x.geometry(c);
                        if (g != null && g[2] * g[3] > area && g[2] > 64) { area = g[2] * g[3]; best = c; }
                    }
                    if (best == 0) continue;
                    if (best != win) { win = best; size[0] = 0; }
                    if (root != null && (root[2] != size[0] || root[3] != size[1])) {
                        size[0] = root[2];
                        size[1] = root[3];
                        x.configure(win, 0, 0, root[2], root[3]);
                    }
                    x.focus(win);
                } catch (Exception e) {
                    if (x != null) x.close();
                    x = null;
                    win = 0;
                }
            }
        });
        t.setDaemon(true);
        t.start();
    }

    /** The few X11 requests a window manager needs, over Xwayland's socket. */
    static class X11 {
        java.nio.channels.SocketChannel ch;
        long root;
        int seq; // requests sent (X11 numbers them from 1)

        static X11 connect(int display) throws IOException {
            X11 x = new X11();
            java.net.UnixDomainSocketAddress a = java.net.UnixDomainSocketAddress.of("@/tmp/.X11-unix/X" + display);
            try {
                x.ch = java.nio.channels.SocketChannel.open(a);
            } catch (IOException e) {
                x.ch = java.nio.channels.SocketChannel.open(java.net.UnixDomainSocketAddress.of("/tmp/.X11-unix/X" + display));
            }
            java.nio.ByteBuffer b = java.nio.ByteBuffer.allocate(12).order(java.nio.ByteOrder.LITTLE_ENDIAN);
            b.put((byte) 'l').put((byte) 0).putShort((short) 11).putShort((short) 0).putShort((short) 0).putShort((short) 0).putShort((short) 0);
            x.write(b.array());
            java.nio.ByteBuffer h = x.read(8);
            if (h.get(0) != 1) throw new IOException("X11 connection refused");
            java.nio.ByteBuffer body = x.read((h.getShort(6) & 0xffff) * 4);
            int vendor = body.getShort(16) & 0xffff;
            int formats = body.get(21) & 0xff;
            int screen = 32 + ((vendor + 3) & ~3) + formats * 8;
            x.root = body.getInt(screen) & 0xffffffffL;
            return x;
        }

        void send(byte[] b) throws IOException {
            seq++;
            write(b);
        }

        void write(byte[] b) throws IOException {
            java.nio.ByteBuffer bb = java.nio.ByteBuffer.wrap(b);
            while (bb.hasRemaining()) ch.write(bb);
        }

        java.nio.ByteBuffer read(int n) throws IOException {
            java.nio.ByteBuffer b = java.nio.ByteBuffer.allocate(n).order(java.nio.ByteOrder.LITTLE_ENDIAN);
            while (b.hasRemaining()) if (ch.read(b) < 0) throw new IOException("X11 connection closed");
            b.flip();
            return b;
        }

        /** Send a request that has a reply; skip events, stop on an error. */
        java.nio.ByteBuffer ask(byte[] req) throws IOException {
            send(req);
            int want = seq & 0xffff;
            while (true) {
                java.nio.ByteBuffer h = read(32);
                int type = h.get(0);
                boolean mine = (h.getShort(2) & 0xffff) == want;
                if (type == 0 && mine) return null; // error (e.g. the window went away)
                if (type == 1) {
                    int extra = h.getInt(4) * 4;
                    java.nio.ByteBuffer all = java.nio.ByteBuffer.allocate(32 + extra).order(java.nio.ByteOrder.LITTLE_ENDIAN);
                    all.put(h.array());
                    if (extra > 0) all.put(read(extra));
                    all.flip();
                    if (mine) return all;
                }
            }
        }

        static byte[] req(int op, int data, int... words) {
            java.nio.ByteBuffer b = java.nio.ByteBuffer.allocate(4 + words.length * 4).order(java.nio.ByteOrder.LITTLE_ENDIAN);
            b.put((byte) op).put((byte) data).putShort((short) (1 + words.length));
            for (int w : words) b.putInt(w);
            return b.array();
        }

        /** {x, y, width, height}, or null. */
        int[] geometry(long w) throws IOException {
            java.nio.ByteBuffer r = ask(req(14, 0, (int) w));
            if (r == null) return null;
            return new int[] {r.getShort(12), r.getShort(14), r.getShort(16) & 0xffff, r.getShort(18) & 0xffff};
        }

        List<Long> children(long w) throws IOException {
            java.nio.ByteBuffer r = ask(req(15, 0, (int) w));
            List<Long> out = new ArrayList<>();
            if (r == null) return out;
            int n = r.getShort(16) & 0xffff;
            for (int i = 0; i < n; i++) out.add(r.getInt(32 + i * 4) & 0xffffffffL);
            return out;
        }

        void configure(long w, int x, int y, int width, int height) throws IOException {
            // ConfigureWindow: x, y, width, height (value mask 0xf).
            send(req(12, 0, (int) w, 0xf, x, y, width, height));
        }

        void focus(long w) throws IOException {
            // SetInputFocus, reverting to PointerRoot, at CurrentTime; then
            // a round trip so errors never pile up unread.
            send(req(42, 1, (int) w, 0));
            ask(req(43, 0));
        }

        void close() {
            try { ch.close(); } catch (IOException e) { }
        }
    }

    // The MayOS launcher app shows the game's own title-screen panorama and
    // logo: copied out of the installed client jar (newest first).
    static final String[][] ART = {
        {"assets/minecraft/textures/gui/title/background/panorama_0.png", "panorama.png"},
        {"assets/minecraft/textures/gui/title/minecraft.png", "logo.png"},
        {"assets/minecraft/textures/gui/title/edition.png", "edition.png"},
    };

    static void extractArt(Path mc, Path jar) {
        try {
            Path out = mc.resolve("mayos-art");
            List<Path> jars = new ArrayList<>();
            if (jar != null) jars.add(jar);
            Path versions = mc.resolve("versions");
            if (Files.isDirectory(versions)) {
                try (var s = Files.list(versions)) {
                    s.map(d -> d.resolve(d.getFileName() + ".jar")).filter(Files::exists)
                        .sorted(Comparator.comparingLong((Path p) -> p.toFile().lastModified()).reversed()).forEach(jars::add);
                }
            }
            Files.createDirectories(out);
            for (String[] a : ART) {
                if (Files.exists(out.resolve(a[1])) && jar == null) continue;
                for (Path j : jars) {
                    try (java.util.zip.ZipFile z = new java.util.zip.ZipFile(j.toFile())) {
                        java.util.zip.ZipEntry e = z.getEntry(a[0]);
                        if (e == null) continue;
                        try (InputStream in = z.getInputStream(e)) {
                            Files.copy(in, out.resolve(a[1]), StandardCopyOption.REPLACE_EXISTING);
                        }
                        break;
                    }
                }
            }
        } catch (Exception e) {
            // only decoration
        }
    }

    // Minecraft pauses singleplayer worlds when its window seems to lose
    // focus; under Xwayland that can happen without a reason, so turn it off.
    static void noPauseOnFocusLoss(Path gameDir) {
        try {
            Path o = gameDir.resolve("options.txt");
            List<String> lines = Files.exists(o) ? new ArrayList<>(Files.readAllLines(o)) : new ArrayList<>();
            lines.removeIf(l -> l.startsWith("pauseOnLostFocus:"));
            lines.add("pauseOnLostFocus:false");
            Files.createDirectories(gameDir);
            Files.write(o, lines);
        } catch (IOException e) {
            // not important enough to stop the game
        }
    }

    static String runOut(String display, String... cmd) throws Exception {
        ProcessBuilder b = new ProcessBuilder(cmd).redirectErrorStream(true);
        b.environment().put("DISPLAY", display);
        Process p = b.start();
        String out = new String(p.getInputStream().readAllBytes());
        p.waitFor();
        return out;
    }

    static String x11Geometry() {
        if (x11Fullscreen && x11Size == null) {
            // The display's size (MayOS: /sys/class/graphics/fb0/virtual_size = "W,H").
            try {
                String[] wh = Files.readString(Paths.get("/sys/class/graphics/fb0/virtual_size")).trim().split(",");
                return Integer.parseInt(wh[0]) + "x" + Integer.parseInt(wh[1]);
            } catch (Exception e) {
                // fall through
            }
        }
        String g = x11Size != null ? x11Size : Optional.ofNullable(System.getenv("MC_X11_SIZE")).orElse("1280x720");
        return g.matches("\\d+x\\d+") ? g : "1280x720";
    }

    // Rootful Xwayland on a free display; the game gets DISPLAY. -shm: no
    // glamor (on vmwgfx its buffer maps fail and the window stays black).
    static Process startXwayland(ProcessBuilder game) throws Exception {
        Files.createDirectories(Paths.get("/tmp/.X11-unix"));
        int d = 7;
        while (Files.exists(Paths.get("/tmp/.X11-unix/X" + d))) d++;
        String geometry = x11Geometry();
        ProcessBuilder xb = new ProcessBuilder(x11Fullscreen
                ? List.of("Xwayland", ":" + d, "-fullscreen", "-geometry", geometry, "-shm", "-ac", "-noreset", "-nolisten", "tcp")
                : List.of("Xwayland", ":" + d, "-geometry", geometry, "-shm", "-ac", "-noreset", "-nolisten", "tcp")).inheritIO();
        xb.environment().putAll(game.environment());
        Process x = xb.start();
        Path sock = Paths.get("/tmp/.X11-unix/X" + d);
        // X clients use the abstract socket; the file may never appear.
        for (int i = 0; i < 30 && !Files.exists(sock) && x.isAlive(); i++) Thread.sleep(100);
        if (!x.isAlive()) throw new RuntimeException("Xwayland did not start (pkg install minecraft installs it)");
        System.out.println("X11: Xwayland on display :" + d + " (" + geometry + ", change with --size 1600x900)");
        game.environment().put("DISPLAY", ":" + d);
        game.environment().remove("WAYLAND_DISPLAY");
        return x;
    }

    // The MayOS performance pack (Modrinth project slugs); your own mods
    // can go next to them in the mods folder.
    static final String[] PACK_MODERN = {"fabric-api", "sodium", "lithium", "ferrite-core", "immediatelyfast", "entityculling",
            "modernfix", "moreculling", "dynamic-fps", "fastload", "clumps", "krypton"};
    static final String[] PACK_LEGACY = {"moehreag-legacy-lwjgl3"};
    static final String[] PACK_FORGE = {"entityculling", "foamfix", "patcher", "ksyxis", "ai-improvements"};

    static void installPack(Path modsDir, String version, boolean legacy, boolean forge) throws Exception {
        Files.createDirectories(modsDir);
        String loaderName = forge ? "forge" : legacy ? "ornithe" : "fabric";
        Map<String, String[]> files = new LinkedHashMap<>(); // project -> url, file name
        Deque<String> todo = new ArrayDeque<>(Arrays.asList(forge ? PACK_FORGE : legacy ? PACK_LEGACY : PACK_MODERN));
        Set<String> seen = new HashSet<>();
        while (!todo.isEmpty()) {
            String project = todo.pop();
            if (!seen.add(project)) continue;
            String q = "https://api.modrinth.com/v2/project/" + project + "/version?loaders=%5B%22" + loaderName
                    + "%22%5D&game_versions=%5B%22" + version + "%22%5D";
            List<Object> vs;
            try {
                vs = list(Json.parse(fetchString(q)));
            } catch (Exception e) {
                vs = List.of();
            }
            if (vs.isEmpty()) {
                System.out.println("  (" + project + ": no build for " + version + ", skipped)");
                continue;
            }
            Map<String, Object> mv = obj(vs.get(0));
            Map<String, Object> file = null;
            for (Object f : list(mv.get("files")))
                if (file == null || Boolean.TRUE.equals(obj(f).get("primary"))) file = obj(f);
            if (file == null) continue;
            files.put(project, new String[] {str(file.get("url")), str(file.get("filename"))});
            for (Object d : list(mv.get("dependencies")))
                if ("required".equals(obj(d).get("dependency_type")) && obj(d).get("project_id") != null) todo.add(str(obj(d).get("project_id")));
        }
        if (forge) {
            // OptiFine (not on Modrinth): its download page carries the link.
            String of = "OptiFine_" + version + "_HD_U_M5.jar";
            try {
                java.util.regex.Matcher m = java.util.regex.Pattern.compile("downloadx\\?f=[^\"' ]+").matcher(fetchString("https://optifine.net/adloadx?f=" + of));
                if (m.find()) files.put("optifine", new String[] {"https://optifine.net/" + m.group().replace("&amp;", "&"), of});
                else System.out.println("  (OptiFine: no download for " + version + ")");
            } catch (Exception e) {
                System.out.println("  (OptiFine: " + e.getMessage() + ")");
            }
        }
        // Replace the jars this pack installed before; leave the user's own.
        Path managed = modsDir.resolve(".mayos-pack");
        Set<String> keep = new HashSet<>();
        for (String[] f : files.values()) keep.add(f[1]);
        if (Files.exists(managed))
            for (String old : Files.readAllLines(managed))
                if (!keep.contains(old)) Files.deleteIfExists(modsDir.resolve(old));
        List<String[]> jobs = new ArrayList<>();
        for (String[] f : files.values()) jobs.add(new String[] {f[0], modsDir.resolve(f[1]).toString(), "-1"});
        System.out.println("Mods: " + String.join(", ", files.keySet()));
        for (String[] j : jobs) download(j[0], Paths.get(j[1]), -2);
        Files.write(managed, keep);
    }

    // Plain strings only: rule objects are for demo mode, custom window
    // sizes, quick play and other operating systems.
    static void addArgs(List<String> cmd, Object args, Map<String, String> vars) {
        for (Object a : list(args)) {
            if (a instanceof String s) cmd.add(subst(s, vars));
            else if (allowed(obj(a).get("rules"))) {
                Object val = obj(a).get("value");
                if (val instanceof String s) cmd.add(subst(s, vars));
                else for (Object s : list(val)) cmd.add(subst(str(s), vars));
            }
        }
    }

    static String subst(String s, Map<String, String> vars) {
        for (Map.Entry<String, String> e : vars.entrySet()) s = s.replace("${" + e.getKey() + "}", e.getValue());
        return s;
    }

    // Library/argument rules: we are Linux x86_64, with no special features.
    static boolean allowed(Object rules) {
        if (rules == null) return true;
        boolean ok = false;
        for (Object r : list(rules)) {
            Map<String, Object> rule = obj(r);
            if (rule.get("features") != null) continue;
            Map<String, Object> os = obj(rule.get("os"));
            boolean match = os == null || ((os.get("name") == null || "linux".equals(os.get("name"))) && (os.get("arch") == null || "x86_64".equals(os.get("arch"))));
            if (match) ok = "allow".equals(rule.get("action"));
        }
        return ok;
    }

    static void downloadAll(List<String[]> jobs) throws Exception {
        List<String[]> todo = new ArrayList<>();
        for (String[] j : jobs) {
            Path p = Paths.get(j[1]);
            long want = Long.parseLong(j[2]);
            if (!Files.exists(p) || (want < 0 ? Files.size(p) == 0 : Files.size(p) != want)) todo.add(j);
        }
        if (todo.isEmpty()) return;
        System.out.println("Downloading " + todo.size() + " files (first start only)...");
        ExecutorService pool = Executors.newFixedThreadPool(8);
        AtomicInteger done = new AtomicInteger();
        List<Future<?>> fs = new ArrayList<>();
        for (String[] j : todo)
            fs.add(pool.submit(() -> {
                download(j[0], Paths.get(j[1]), Long.parseLong(j[2]));
                int n = done.incrementAndGet();
                if (n % 200 == 0 || n == todo.size()) System.out.println("  " + n + " / " + todo.size());
                return null;
            }));
        for (Future<?> f : fs) f.get();
        pool.shutdown();
    }

    static void download(String url, Path to, long size) throws Exception {
        if (Files.exists(to) && (size < 0 ? Files.size(to) > 0 : Files.size(to) == size)) return;
        Files.createDirectories(to.getParent());
        Path tmp = to.resolveSibling(to.getFileName() + ".part");
        for (int attempt = 1; ; attempt++) {
            try {
                HttpResponse<Path> r = HTTP.send(HttpRequest.newBuilder(URI.create(url)).build(), HttpResponse.BodyHandlers.ofFile(tmp));
                if (r.statusCode() != 200) throw new IOException("HTTP " + r.statusCode() + " for " + url);
                Files.move(tmp, to, StandardCopyOption.REPLACE_EXISTING);
                return;
            } catch (IOException e) {
                if (attempt == 3) throw e;
            }
        }
    }

    // Metadata is cached, so the launcher also starts offline.
    static Path cacheDir;

    static String fetchString(String url) throws Exception {
        Path cached = cacheDir == null ? null : cacheDir.resolve(Integer.toHexString(url.hashCode()) + ".txt");
        try {
            HttpResponse<String> r = HTTP.send(HttpRequest.newBuilder(URI.create(url)).timeout(java.time.Duration.ofSeconds(20)).build(), HttpResponse.BodyHandlers.ofString());
            if (r.statusCode() != 200) throw new IOException("HTTP " + r.statusCode());
            if (cached != null) {
                Files.createDirectories(cacheDir);
                Files.writeString(cached, r.body());
            }
            return r.body();
        } catch (Exception e) {
            if (cached != null && Files.exists(cached)) return Files.readString(cached);
            throw e;
        }
    }

    @SuppressWarnings("unchecked")
    static Map<String, Object> obj(Object o) { return (Map<String, Object>) o; }
    @SuppressWarnings("unchecked")
    static List<Object> list(Object o) { return o == null ? List.of() : (List<Object>) o; }
    static String str(Object o) { return (String) o; }
    static long num(Object o) { return o == null ? -1 : ((Number) o).longValue(); }

    // A small JSON reader (objects, arrays, strings, numbers, literals).
    static class Json {
        final String s;
        int i;
        Json(String s) { this.s = s; }
        static Object parse(String s) { return new Json(s).value(); }
        void ws() { while (i < s.length() && Character.isWhitespace(s.charAt(i))) i++; }
        Object value() {
            ws();
            char c = s.charAt(i);
            if (c == '{') {
                Map<String, Object> m = new LinkedHashMap<>();
                i++; ws();
                if (s.charAt(i) == '}') { i++; return m; }
                while (true) {
                    ws(); String k = string(); ws(); i++; // ':'
                    m.put(k, value()); ws();
                    if (s.charAt(i++) == '}') return m;
                }
            }
            if (c == '[') {
                List<Object> l = new ArrayList<>();
                i++; ws();
                if (s.charAt(i) == ']') { i++; return l; }
                while (true) {
                    l.add(value()); ws();
                    if (s.charAt(i++) == ']') return l;
                }
            }
            if (c == '"') return string();
            if (s.startsWith("true", i)) { i += 4; return true; }
            if (s.startsWith("false", i)) { i += 5; return false; }
            if (s.startsWith("null", i)) { i += 4; return null; }
            int st = i;
            while (i < s.length() && "+-0123456789.eE".indexOf(s.charAt(i)) >= 0) i++;
            String n = s.substring(st, i);
            return n.matches("-?\\d+") ? (Object) Long.parseLong(n) : (Object) Double.parseDouble(n);
        }
        String string() {
            StringBuilder b = new StringBuilder();
            i++; // opening quote
            while (true) {
                char c = s.charAt(i++);
                if (c == '"') return b.toString();
                if (c == '\\') {
                    char e = s.charAt(i++);
                    switch (e) {
                        case 'n' -> b.append('\n');
                        case 't' -> b.append('\t');
                        case 'r' -> b.append('\r');
                        case 'b' -> b.append('\b');
                        case 'f' -> b.append('\f');
                        case 'u' -> { b.append((char) Integer.parseInt(s.substring(i, i + 4), 16)); i += 4; }
                        default -> b.append(e);
                    }
                } else b.append(c);
            }
        }
    }
}

/**
 * Microsoft account sign-in (device code) and the Xbox Live / Minecraft
 * services token exchange. Needs an Azure app ID in /etc/minecraft-client-id
 * that Microsoft has approved for Minecraft.
 */
class Account {
    String name, uuid, token, refresh;
    long expires;

    // MayOS's default Microsoft app ID; /etc/minecraft-client-id overrides it.
    static final String DEFAULT_CLIENT_ID = "4828c89c-ee13-40fa-aa8a-36fb6ad65f60";

    static String clientId() throws IOException {
        Path p = Paths.get("/etc/minecraft-client-id");
        String id = Files.exists(p) ? Files.readString(p).trim() : "";
        return id.isEmpty() ? DEFAULT_CLIENT_ID : id;
    }

    static String form(Map<String, String> m) {
        StringBuilder b = new StringBuilder();
        for (var e : m.entrySet()) {
            if (b.length() > 0) b.append('&');
            b.append(java.net.URLEncoder.encode(e.getKey(), java.nio.charset.StandardCharsets.UTF_8)).append('=')
                .append(java.net.URLEncoder.encode(e.getValue(), java.nio.charset.StandardCharsets.UTF_8));
        }
        return b.toString();
    }

    static Map<String, Object> post(String url, String body, String type) throws Exception {
        HttpRequest.Builder r = HttpRequest.newBuilder(URI.create(url)).header("Content-Type", type).header("Accept", "application/json")
            .POST(HttpRequest.BodyPublishers.ofString(body));
        HttpResponse<String> res = Launcher.HTTP.send(r.build(), HttpResponse.BodyHandlers.ofString());
        Object j = Launcher.Json.parse(res.body().isEmpty() ? "{}" : res.body());
        @SuppressWarnings("unchecked") Map<String, Object> m = (Map<String, Object>) j;
        if (res.statusCode() >= 400 && m.get("error") == null) m.put("error", "HTTP " + res.statusCode() + " " + res.body());
        return m;
    }

    static String esc(String s) {
        return s.replace("\\", "\\\\").replace("\"", "\\\"");
    }

    /** Microsoft token -> Xbox Live -> XSTS -> Minecraft token and profile. */
    static Account finish(String msToken, String refresh, long msExpires) throws Exception {
        Map<String, Object> xbl = post("https://user.auth.xboxlive.com/user/authenticate",
            "{\"Properties\":{\"AuthMethod\":\"RPS\",\"SiteName\":\"user.auth.xboxlive.com\",\"RpsTicket\":\"d=" + esc(msToken)
                + "\"},\"RelyingParty\":\"http://auth.xboxlive.com\",\"TokenType\":\"JWT\"}", "application/json");
        String xblToken = (String) xbl.get("Token");
        if (xblToken == null) throw new IOException("Xbox Live sign-in failed: " + xbl);
        Map<String, Object> xsts = post("https://xsts.auth.xboxlive.com/xsts/authorize",
            "{\"Properties\":{\"SandboxId\":\"RETAIL\",\"UserTokens\":[\"" + esc(xblToken)
                + "\"]},\"RelyingParty\":\"rp://api.minecraftservices.com/\",\"TokenType\":\"JWT\"}", "application/json");
        String xstsToken = (String) xsts.get("Token");
        if (xstsToken == null) throw new IOException("Xbox sign-in refused (no Xbox profile, or a child account?): " + xsts);
        @SuppressWarnings("unchecked") Map<String, Object> claims = (Map<String, Object>) xsts.get("DisplayClaims");
        @SuppressWarnings("unchecked") Map<String, Object> xui = (Map<String, Object>) ((List<Object>) claims.get("xui")).get(0);
        String uhs = (String) xui.get("uhs");
        Map<String, Object> mc = post("https://api.minecraftservices.com/authentication/login_with_xbox",
            "{\"identityToken\":\"XBL3.0 x=" + uhs + ";" + esc(xstsToken) + "\"}", "application/json");
        String mcToken = (String) mc.get("access_token");
        if (mcToken == null) throw new IOException("Minecraft sign-in failed (is the app ID approved for Minecraft?): " + mc);
        HttpResponse<String> prof = Launcher.HTTP.send(HttpRequest.newBuilder(URI.create("https://api.minecraftservices.com/minecraft/profile"))
            .header("Authorization", "Bearer " + mcToken).build(), HttpResponse.BodyHandlers.ofString());
        @SuppressWarnings("unchecked") Map<String, Object> p = (Map<String, Object>) Launcher.Json.parse(prof.body());
        if (p.get("id") == null) throw new IOException("this Microsoft account does not own Minecraft Java Edition");
        Account a = new Account();
        a.name = (String) p.get("name");
        a.uuid = (String) p.get("id");
        a.token = mcToken;
        a.refresh = refresh;
        a.expires = msExpires;
        return a;
    }

    void save(Path f) throws IOException {
        Files.writeString(f, "{\"name\":\"" + esc(name) + "\",\"uuid\":\"" + uuid + "\",\"token\":\"" + esc(token)
            + "\",\"refresh\":\"" + esc(refresh) + "\",\"expires\":" + expires + "}");
    }

    static void login(Path f) throws Exception {
        String id = clientId();
        Map<String, Object> dc = post("https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode",
            form(Map.of("client_id", id, "scope", "XboxLive.signin offline_access")), "application/x-www-form-urlencoded");
        if (dc.get("user_code") == null) throw new IOException("Microsoft sign-in could not start: " + dc);
        System.out.println();
        System.out.println("  On your phone or another computer, open:  " + dc.get("verification_uri"));
        System.out.println("  and enter the code:  " + dc.get("user_code"));
        System.out.println("MAYOS-LOGIN " + dc.get("verification_uri") + " " + dc.get("user_code"));
        System.out.println();
        long interval = Math.max(5, ((Number) dc.get("interval")).longValue());
        long deadline = System.currentTimeMillis() + ((Number) dc.get("expires_in")).longValue() * 1000;
        while (System.currentTimeMillis() < deadline) {
            Thread.sleep(interval * 1000);
            Map<String, Object> t = post("https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
                form(Map.of("grant_type", "urn:ietf:params:oauth:grant-type:device_code", "client_id", id, "device_code", (String) dc.get("device_code"))),
                "application/x-www-form-urlencoded");
            Object err = t.get("error");
            if ("authorization_pending".equals(err)) continue;
            if ("slow_down".equals(err)) { interval += 5; continue; }
            if (err != null) throw new IOException("Microsoft sign-in failed: " + t.get("error_description"));
            long exp = System.currentTimeMillis() + ((Number) t.get("expires_in")).longValue() * 1000;
            Account a = finish((String) t.get("access_token"), (String) t.get("refresh_token"), exp);
            a.save(f);
            System.out.println("Signed in as " + a.name + ". Play from the Minecraft launcher.");
            System.out.println("MAYOS-LOGIN-OK " + a.name);
            return;
        }
        throw new IOException("the code expired: try again");
    }

    /** The saved account, refreshed when its token is old. */
    static Account load(Path f) {
        try {
            @SuppressWarnings("unchecked") Map<String, Object> m = (Map<String, Object>) Launcher.Json.parse(Files.readString(f));
            Account a = new Account();
            a.name = (String) m.get("name");
            a.uuid = (String) m.get("uuid");
            a.token = (String) m.get("token");
            a.refresh = (String) m.get("refresh");
            a.expires = ((Number) m.get("expires")).longValue();
            if (System.currentTimeMillis() < a.expires - 60_000) return a;
            Map<String, Object> t = post("https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
                form(Map.of("grant_type", "refresh_token", "client_id", clientId(), "refresh_token", a.refresh, "scope", "XboxLive.signin offline_access")),
                "application/x-www-form-urlencoded");
            if (t.get("access_token") == null) throw new IOException("sign in again (" + t.get("error") + ")");
            long exp = System.currentTimeMillis() + ((Number) t.get("expires_in")).longValue() * 1000;
            Account n = finish((String) t.get("access_token"), (String) t.getOrDefault("refresh_token", a.refresh), exp);
            n.save(f);
            return n;
        } catch (Exception e) {
            System.out.println("Microsoft account: " + e.getMessage() + "; playing offline.");
            return null;
        }
    }
}
