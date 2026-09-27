// MayOS's Minecraft launcher: downloads Minecraft: Java Edition from
// Mojang's servers and starts it in offline mode (singleplayer).
//
//   minecraft [version] [--user NAME] [--memory 4G] [--mods | --vanilla | --forge] [--software] [--size WxH] [--dry-run] [--debug]
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
    static final String BUILD = "2026-09-27b";
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
        boolean packOnly = false, forge = false, software = false, forceGpu = false;
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
            else if (args[i].equals("--software")) software = true;
            else if (args[i].equals("--gpu")) forceGpu = true;
            else if (args[i].equals("--size") && i + 1 < args.length) x11Size = args[++i];
            else version = args[i];
        }
        Path home = Paths.get(System.getProperty("user.home", "/home"));
        if (!Files.isWritable(home)) home = Paths.get("/home");
        Path mc = home.resolve(".minecraft");
        Path store = trustStore(mc);
        HTTP = HttpClient.newBuilder().followRedirects(HttpClient.Redirect.NORMAL).build();
        cacheDir = mc.resolve("cache");

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
                    ? "fancyGraphics:false\nao:0\n"
                    : "graphicsMode:0\nao:false\nsimulationDistance:5\nbiomeBlendRadius:0\nrenderClouds:\"false\"\n"));
            if (forge) Files.writeString(gameDir.resolve("optionsof.txt"),
                    "ofFastRender:true\nofFastMath:true\nofSmoothFps:false\nofChunkUpdates:2\nofChunkUpdatesDynamic:true\nofAaLevel:0\nofAfLevel:1\nofClouds:3\nofTrees:1\nofDroppedItems:1\nofRainSplash:false\nofAnimatedWater:1\nofAnimatedLava:1\nofVignette:1\nofSky:true\nofDynamicFov:false\n");
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
        vars.put("auth_session", "0");
        vars.put("clientid", "0");
        vars.put("auth_xuid", "0");
        vars.put("user_type", "legacy");
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
        int code = pb.start().waitFor();
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

    static String x11Geometry() {
        String g = x11Size != null ? x11Size : Optional.ofNullable(System.getenv("MC_X11_SIZE")).orElse("1280x720");
        return g.matches("\\d+x\\d+") ? g : "1280x720";
    }

    // Rootful Xwayland on a free display; the game gets DISPLAY.
    static Process startXwayland(ProcessBuilder game) throws Exception {
        Files.createDirectories(Paths.get("/tmp/.X11-unix"));
        int d = 7;
        while (Files.exists(Paths.get("/tmp/.X11-unix/X" + d))) d++;
        String geometry = x11Geometry();
        ProcessBuilder xb = new ProcessBuilder("Xwayland", ":" + d, "-geometry", geometry, "-ac", "-noreset", "-nolisten", "tcp").inheritIO();
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
