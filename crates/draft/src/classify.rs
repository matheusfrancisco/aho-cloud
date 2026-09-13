//! What a simple command is: a build, a server, or neither; and which port a server names.

use crate::shell::{Segment, basename};

/// A recognised build step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BuildStep {
    /// `docker build` / `docker buildx build` / `podman build`: the Dockerfile and context.
    Dockerfile { path: String, context: String },
    /// A toolchain build run as a command build; `image` is the usual base image for it.
    Command { argv: Vec<String>, image: &'static str, toolchain: &'static str },
}

/// Docker flags that take a separate value.
const DOCKER_VALUE_FLAGS: &[&str] = &[
    "-f",
    "--file",
    "-t",
    "--tag",
    "--build-arg",
    "--target",
    "--platform",
    "--label",
    "--secret",
    "--ssh",
    "--cache-from",
    "--cache-to",
    "--network",
    "--progress",
    "-o",
    "--output",
    "--iidfile",
    "--add-host",
    "--build-context",
    "--shm-size",
    "--ulimit",
    "-m",
    "--memory",
    "--cpuset-cpus",
    "--metadata-file",
    "--builder",
    "--attest",
    "--annotation",
    "--cgroup-parent",
];

pub(crate) fn build_step(seg: &Segment) -> Option<BuildStep> {
    let argv = &seg.argv;
    let prog = basename(argv.first()?);
    let arg = |i: usize| argv.get(i).map(String::as_str);
    match prog {
        "docker" | "podman" => {
            let rest = match (arg(1), arg(2)) {
                (Some("build"), _) => &argv[2..],
                (Some("buildx" | "image"), Some("build")) => &argv[3..],
                _ => return None,
            };
            docker_build(rest)
        }
        "npm" | "pnpm" | "yarn" => {
            let script = match (arg(1), arg(2)) {
                (Some("run" | "run-script"), Some(s)) => s,
                (Some(s), _) if prog != "npm" => s,
                _ => return None,
            };
            script.starts_with("build").then(|| command(argv, "node:22", "node"))
        }
        "cargo" => {
            let sub = argv[1..].iter().find(|a| !a.starts_with('+'))?;
            (sub == "build").then(|| command(argv, "rust:1", "cargo"))
        }
        "go" => (arg(1) == Some("build")).then(|| command(argv, "golang:1", "go")),
        "pip" | "pip3" => (arg(1) == Some("install")).then(|| command(argv, "python:3", "pip")),
        "python" | "python3" => {
            (arg(1) == Some("-m") && arg(2) == Some("pip") && arg(3) == Some("install")).then(|| command(argv, "python:3", "pip"))
        }
        "mvn" | "mvnw" => argv[1..]
            .iter()
            .any(|a| matches!(a.as_str(), "package" | "install" | "verify"))
            .then(|| command(argv, "maven:3-eclipse-temurin-21", "maven")),
        "gradle" | "gradlew" => argv[1..]
            .iter()
            .any(|a| matches!(a.as_str(), "build" | "assemble" | "bootJar" | "jar" | "installDist" | "shadowJar"))
            .then(|| command(argv, "gradle:8-jdk21", "gradle")),
        _ => None,
    }
}

fn command(argv: &[String], image: &'static str, toolchain: &'static str) -> BuildStep {
    BuildStep::Command { argv: argv.to_vec(), image, toolchain }
}

fn docker_build(args: &[String]) -> Option<BuildStep> {
    let mut file = None;
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--file=") {
            file = Some(v.to_owned());
        } else if a == "-f" || a == "--file" {
            file = it.next().cloned();
        } else if DOCKER_VALUE_FLAGS.contains(&a.as_str()) {
            it.next();
        } else if !a.starts_with('-') {
            positional.push(a.clone());
        }
    }
    let context = positional.pop()?;
    if context == "-" || context.contains("://") {
        return None;
    }
    let context = normalize(&context);
    let path = match file {
        Some(f) => normalize(&f),
        None if context == "." => "Dockerfile".to_owned(),
        None => format!("{context}/Dockerfile"),
    };
    Some(BuildStep::Dockerfile { path, context })
}

/// `./app/` → `app`; `.` stays.
fn normalize(p: &str) -> String {
    let mut s = p.trim_end_matches('/');
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    if s.is_empty() { ".".to_owned() } else { s.to_owned() }
}

/// Is `p` a path relative to the source root that stays inside it?
pub(crate) fn inside_source(p: &str) -> bool {
    !p.starts_with('/') && !p.starts_with('~') && p.split('/').all(|c| c != "..")
}

/// Does the command start something that serves requests?
pub(crate) fn is_server(seg: &Segment) -> bool {
    let argv = &seg.argv;
    let Some(first) = argv.first() else { return false };
    let prog = basename(first);
    let arg = |i: usize| argv.get(i).map(String::as_str);
    let script_is_server = |s: Option<&str>| s.is_some_and(|s| s.starts_with("start") || matches!(s, "serve" | "preview" | "dev"));
    match prog {
        "npm" | "pnpm" | "yarn" | "bun" => match arg(1) {
            Some("start") => true,
            Some("run" | "run-script") => script_is_server(arg(2)) || (prog == "bun" && arg(2).is_some_and(is_script_file)),
            Some(s) if prog != "npm" => script_is_server(Some(s)) || (prog == "bun" && is_script_file(s)),
            _ => false,
        },
        "npx" | "pnpx" | "bunx" => argv[1..].first().is_some_and(|s| is_node_server_tool(s, arg(2))),
        "next" | "vite" | "serve" | "http-server" | "nuxt" | "astro" | "remix-serve" => is_node_server_tool(prog, arg(1)),
        "node" | "nodemon" | "tsx" | "ts-node" => argv[1..].iter().find(|a| !a.starts_with('-')).is_some_and(|a| is_script_file(a)),
        "deno" => matches!(arg(1), Some("run" | "serve" | "task")),
        "python" | "python3" => match arg(1) {
            Some("-m") => {
                matches!(arg(2), Some("http.server" | "uvicorn" | "gunicorn" | "hypercorn" | "flask" | "streamlit" | "fastapi" | "django"))
            }
            Some("manage.py") => arg(2) == Some("runserver"),
            Some(f) => f.ends_with(".py"),
            None => false,
        },
        "uvicorn" | "gunicorn" | "hypercorn" | "daphne" | "waitress-serve" | "puma" | "rackup" | "unicorn" => true,
        "flask" => arg(1) == Some("run"),
        "fastapi" => matches!(arg(1), Some("run" | "dev")),
        "streamlit" => arg(1) == Some("run"),
        "rails" => matches!(arg(1), Some("s" | "server")),
        "bundle" => arg(1) == Some("exec") && is_server(&Segment { argv: argv[2..].to_vec(), ..Segment::default() }),
        "cargo" => argv[1..].iter().find(|a| !a.starts_with('+')).is_some_and(|s| s == "run"),
        "go" => arg(1) == Some("run"),
        "java" => argv[1..].iter().any(|a| a == "-jar"),
        "php" => arg(1) == Some("-S") || (arg(1) == Some("artisan") && arg(2) == Some("serve")),
        "dotnet" => arg(1) == Some("run"),
        _ => first.contains("target/release/") || first.contains("target/debug/"),
    }
}

fn is_script_file(s: &str) -> bool {
    [".js", ".mjs", ".cjs", ".ts", ".mts", ".jsx", ".tsx"].iter().any(|ext| s.ends_with(ext))
}

fn is_node_server_tool(tool: &str, sub: Option<&str>) -> bool {
    match tool {
        "next" | "nuxt" | "astro" => matches!(sub, Some("start" | "dev" | "preview")),
        "vite" => matches!(sub, None | Some("preview" | "dev" | "serve")) || sub.is_some_and(|s| s.starts_with('-')),
        "serve" | "http-server" | "remix-serve" => true,
        _ => false,
    }
}

/// The port a server command names: `PORT=`, `--port`, `-p`, `--bind`/`-b host:port`,
/// `--listen`/`-l`, `php -S host:port`, `python -m http.server PORT`.
pub(crate) fn port(seg: &Segment) -> Option<u16> {
    if let Some(p) = seg.assigns.iter().find(|(k, _)| k == "PORT").and_then(|(_, v)| v.parse().ok()) {
        return Some(p);
    }
    let argv = &seg.argv;
    let mut it = argv.iter().skip(1).peekable();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v.to_owned())),
            _ => (a.as_str(), None),
        };
        let takes = matches!(flag, "--port" | "-p" | "--bind" | "-b" | "--listen" | "-l" | "-S" | "--http-port");
        if !takes {
            continue;
        }
        let value = match inline {
            Some(v) => v,
            None => match it.next() {
                Some(v) => v.clone(),
                None => break,
            },
        };
        if let Some(p) = value.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).filter(|p| *p > 0) {
            return Some(p);
        }
    }
    let prog = argv.first().map(|a| basename(a));
    if matches!(prog, Some("python" | "python3")) && argv.get(2).map(String::as_str) == Some("http.server") {
        return argv.get(3).and_then(|p| p.parse().ok());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::segments;

    fn one(line: &str) -> Segment {
        segments(line).remove(0)
    }

    #[test]
    fn docker_builds_name_file_and_context() {
        assert_eq!(
            build_step(&one("docker build -t app:dev --build-arg X=1 .")),
            Some(BuildStep::Dockerfile { path: "Dockerfile".into(), context: ".".into() })
        );
        assert_eq!(
            build_step(&one("docker buildx build --platform linux/amd64 -f ./deploy/Dockerfile.prod ./services/api/")),
            Some(BuildStep::Dockerfile { path: "deploy/Dockerfile.prod".into(), context: "services/api".into() })
        );
        assert_eq!(
            build_step(&one("docker build ./web")),
            Some(BuildStep::Dockerfile { path: "web/Dockerfile".into(), context: "web".into() })
        );
        assert_eq!(build_step(&one("docker build -t x -")), None, "stdin context is no source build");
        assert_eq!(build_step(&one("docker run -p 8080:80 nginx")), None);
    }

    #[test]
    fn toolchain_builds_are_command_builds() {
        let image = |l: &str| match build_step(&one(l)) {
            Some(BuildStep::Command { image, .. }) => Some(image),
            _ => None,
        };
        assert_eq!(image("npm run build"), Some("node:22"));
        assert_eq!(image("pnpm build:prod"), Some("node:22"));
        assert_eq!(image("yarn build"), Some("node:22"));
        assert_eq!(image("cargo +nightly build --release"), Some("rust:1"));
        assert_eq!(image("go build ./..."), Some("golang:1"));
        assert_eq!(image("pip install -r requirements.txt"), Some("python:3"));
        assert_eq!(image("python3 -m pip install ."), Some("python:3"));
        assert_eq!(image("./mvnw -q package"), Some("maven:3-eclipse-temurin-21"));
        assert_eq!(image("./gradlew bootJar"), Some("gradle:8-jdk21"));
        for not_build in ["npm install", "npm test", "cargo test", "go test ./...", "pip list", "mvn test", "gradle test", "npm run lint"] {
            assert_eq!(image(not_build), None, "{not_build}");
        }
    }

    #[test]
    fn servers_are_recognised() {
        for s in [
            "npm start",
            "npm run start:prod",
            "yarn dev",
            "bun run server.ts",
            "node dist/server.js",
            "npx next start -p 4000",
            "python app.py",
            "python -m uvicorn main:app",
            "uvicorn main:app --port 8000",
            "gunicorn -b 0.0.0.0:8000 app:app",
            "flask run",
            "bundle exec rails s",
            "cargo run --release",
            "./target/release/api",
            "go run ./cmd/api",
            "java -jar app.jar",
            "php -S 0.0.0.0:8080",
            "deno run -A main.ts",
        ] {
            assert!(is_server(&one(s)), "{s}");
        }
        for s in ["npm test", "node --version", "python -m pytest", "cargo build", "ls -la", "curl http://localhost:3000"] {
            assert!(!is_server(&one(s)), "{s}");
        }
    }

    #[test]
    fn ports_come_from_flags_and_env() {
        assert_eq!(port(&one("PORT=4000 node server.js")), Some(4000));
        assert_eq!(port(&one("uvicorn main:app --port 8001")), Some(8001));
        assert_eq!(port(&one("next start --port=3100")), Some(3100));
        assert_eq!(port(&one("gunicorn -b 0.0.0.0:8002 app:app")), Some(8002));
        assert_eq!(port(&one("php -S localhost:8003")), Some(8003));
        assert_eq!(port(&one("python3 -m http.server 8004")), Some(8004));
        assert_eq!(port(&one("npm start")), None);
    }
}
