// MayOS's Minecraft launcher: downloads Minecraft: Java Edition from
// Mojang's servers and starts it in offline mode (singleplayer).
//
//   minecraft [version] [--user NAME] [--dry-run]
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
        boolean dry = false;
        for (int i = 0; i < args.length; i++) {
            if (args[i].equals("--user") && i + 1 < args.length) user = args[++i];
            else if (args[i].equals("--dry-run")) dry = true;
            else version = args[i];
        }
        Path home = Paths.get(System.getProperty("user.home", "/home"));
        if (!Files.isWritable(home)) home = Paths.get("/home");
        Path mc = home.resolve(".minecraft");
        Path store = trustStore(mc);
        HTTP = HttpClient.newBuilder().followRedirects(HttpClient.Redirect.NORMAL).build();

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

        // Client jar and libraries.
        List<String[]> jobs = new ArrayList<>(); // url, path, size
        Path client = vdir.resolve(version + ".jar");
        Map<String, Object> cd = obj(obj(v.get("downloads")).get("client"));
        jobs.add(new String[] {str(cd.get("url")), client.toString(), String.valueOf(num(cd.get("size")))});
        List<String> cp = new ArrayList<>();
        for (Object lo : list(v.get("libraries"))) {
            Map<String, Object> lib = obj(lo);
            if (!allowed(lib.get("rules"))) continue;
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
        if (!dry) {
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
        Map<String, String> vars = new HashMap<>();
        vars.put("auth_player_name", user);
        vars.put("version_name", version);
        vars.put("game_directory", mc.toString());
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
        List<String> cmd = new ArrayList<>();
        cmd.add(java);
        if (store != null) {
            cmd.add("-Djavax.net.ssl.trustStore=" + store);
            cmd.add("-Djavax.net.ssl.trustStoreType=PKCS12");
            cmd.add("-Djavax.net.ssl.trustStorePassword=changeit");
        }
        cmd.add("-Xmx" + Optional.ofNullable(System.getenv("MC_MEMORY")).orElse("1G"));
        // Use Alpine's (musl) GLFW, OpenAL and Mesa instead of the glibc
        // builds inside Mojang's LWJGL jars.
        for (String[] l : new String[][] {{"glfw", "/usr/lib/libglfw.so.3"}, {"openal", "/usr/lib/libopenal.so.1"}, {"opengl", "/usr/lib/libGL.so.1"}})
            if (Files.exists(Paths.get(l[1]))) cmd.add("-Dorg.lwjgl." + l[0] + ".libname=" + l[1]);
        Map<String, Object> arguments = obj(v.get("arguments"));
        if (arguments != null) {
            addArgs(cmd, arguments.get("jvm"), vars);
            cmd.add(str(v.get("mainClass")));
            addArgs(cmd, arguments.get("game"), vars);
        } else {
            // Versions before 1.13.
            cmd.add("-Djava.library.path=" + natives);
            cmd.add("-cp");
            cmd.add(vars.get("classpath"));
            cmd.add(str(v.get("mainClass")));
            for (String a : str(v.get("minecraftArguments")).split(" ")) cmd.add(subst(a, vars));
        }
        if (dry) {
            System.out.println(String.join(" ", cmd));
            return;
        }
        System.out.println("Starting Minecraft...");
        ProcessBuilder pb = new ProcessBuilder(cmd).directory(mc.toFile()).inheritIO();
        pb.environment().put("XDG_SESSION_TYPE", "wayland");
        pb.environment().remove("DISPLAY");
        System.exit(pb.start().waitFor());
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
            if (!Files.exists(p) || Files.size(p) != Long.parseLong(j[2])) todo.add(j);
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
        if (Files.exists(to) && (size < 0 ? to.toString().endsWith(".json") && Files.size(to) > 0 : Files.size(to) == size)) return;
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

    static String fetchString(String url) throws Exception {
        return HTTP.send(HttpRequest.newBuilder(URI.create(url)).build(), HttpResponse.BodyHandlers.ofString()).body();
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
