use mage_core::domain::{Language, LineRange, RepoPath};
use mage_core::ports::knowledge::{AppFact, FrameworkId, FrameworkScope, HandlerRef, SymbolDef, SymbolKind, TableOp, UrlPart, UrlTemplate};

use super::{detect_frameworks, extract};

fn parse(lang: Language, src: &str) -> tree_sitter::Tree {
    let language: tree_sitter::Language = match lang {
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Clojure | Language::ClojureScript | Language::Cljc => tree_sitter_clojure_orchard::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        other => panic!("no grammar for {other:?}"),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap_or_else(|e| panic!("set_language: {e}"));
    parser.parse(src, None).unwrap_or_else(|| panic!("parse failed"))
}

fn repo_path(p: &str) -> RepoPath {
    RepoPath::new(p).unwrap_or_else(|e| panic!("bad path {p}: {e:?}"))
}

/// (line, fact) pairs.
type Facts = Vec<(u32, AppFact)>;

/// (line, fact) of `src` for one framework.
fn facts_at(lang: Language, path: &str, src: &str, fw: FrameworkId, symbols: &[SymbolDef]) -> Facts {
    let tree = parse(lang, src);
    let records = extract(lang, &repo_path(path), &tree, src.as_bytes(), symbols, &[fw]);
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r.ord as usize, i, "ords are dense and ordered");
        assert_eq!(r.framework, fw);
    }
    records.into_iter().map(|r| (r.line, r.fact)).collect()
}

fn facts(lang: Language, path: &str, src: &str, fw: FrameworkId) -> Facts {
    facts_at(lang, path, src, fw, &[])
}

fn func(ord: u32, name: &str, start: u32, end: u32) -> SymbolDef {
    SymbolDef {
        ord,
        name: name.to_owned(),
        qualified: name.to_owned(),
        kind: SymbolKind::Function,
        container: None,
        span: LineRange::new(start, end).unwrap_or_else(|e| panic!("{e:?}")),
        name_pos: (start, 1),
        signature: None,
        doc: None,
        exported: true,
    }
}

fn lit(s: &str) -> UrlPart {
    UrlPart::Lit(s.to_owned())
}

fn var(s: &str) -> UrlPart {
    UrlPart::Var(Some(s.to_owned()))
}

fn tpl(parts: Vec<UrlPart>) -> UrlTemplate {
    UrlTemplate { parts }
}

fn path(s: &str) -> UrlTemplate {
    tpl(vec![lit(s)])
}

fn group(var: &str, prefix: &str, parent: &str) -> AppFact {
    let prefix = if prefix.is_empty() { tpl(vec![]) } else { path(prefix) };
    AppFact::RouteGroup { var: var.to_owned(), prefix, parent: Some(parent.to_owned()) }
}

fn named(name: &str, qualifier: Option<&str>) -> HandlerRef {
    HandlerRef::Named { name: name.to_owned(), qualifier: qualifier.map(str::to_owned) }
}

fn decl(method: Option<&str>, p: UrlTemplate, group: Option<&str>, handler: HandlerRef, middleware: &[&str]) -> AppFact {
    AppFact::RouteDecl {
        method: method.map(str::to_owned),
        path: p,
        group: group.map(str::to_owned),
        handler,
        middleware: middleware.iter().map(|m| (*m).to_owned()).collect(),
    }
}

fn page(p: &str, component: &str) -> AppFact {
    AppFact::PageDecl { path: path(p), component: component.to_owned() }
}

fn call(method: Option<&str>, url: Vec<UrlPart>, client: &str) -> AppFact {
    AppFact::HttpCall { method: method.map(str::to_owned), url: tpl(url), client: client.to_owned() }
}

fn table(name: &str, op: TableOp) -> AppFact {
    AppFact::TableRef { table: name.to_owned(), op }
}

// ---------------------------------------------------------------------------------------------
// Go

const GIN_SRC: &str = r#"package api

func Register(r *gin.RouterGroup, h *Handler) {
	api := r.Group("/api", auth.Required())
	api.GET("/items/:id", audit, h.GetItem)
	api.POST("/items", h.CreateItem)
	admin.Register(api)
	api.Any("/proxy/*path", proxy.Handler(cfg))
	api.Handle("PATCH", "/items/:id", h.Patch)
}

func Routes(rg *gin.RouterGroup, list gin.HandlerFunc) {
	rg.DELETE("/x", list)
	rg.GET("/y", func(c *gin.Context) {})
}

func main() {
	r := gin.Default()
	SetupRoutes(r)
	r.Run(":8080")
	cache.Get("key", v)
}
"#;

#[test]
fn app_map_gin_groups_middleware_params_and_pass_sites() {
    let symbols = [func(0, "Register", 3, 10), func(1, "Routes", 12, 15), func(2, "main", 17, 22)];
    let got = facts_at(Language::Go, "api/routes.go", GIN_SRC, FrameworkId::Gin, &symbols);
    let want = vec![
        (3, group("r", "", "Register()")),
        (4, group("api", "/api", "r")),
        (5, decl(Some("GET"), path("/items/:id"), Some("api"), named("GetItem", Some("h")), &["audit"])),
        (6, decl(Some("POST"), path("/items"), Some("api"), named("CreateItem", Some("h")), &[])),
        (7, group("Register()", "", "api")),
        (8, decl(None, path("/proxy/*path"), Some("api"), named("Handler", Some("proxy")), &[])),
        (9, decl(Some("PATCH"), path("/items/:id"), Some("api"), named("Patch", Some("h")), &[])),
        (12, group("rg", "", "Routes()")),
        (13, decl(Some("DELETE"), path("/x"), Some("rg"), HandlerRef::Param { index: 1 }, &[])),
        (14, decl(Some("GET"), path("/y"), Some("rg"), HandlerRef::Inline { enclosing: Some(1), line: 14 }, &[])),
        (18, group("r", "", "main()")),
        (19, group("SetupRoutes()", "", "r")),
    ];
    assert_eq!(got, want);

    let tree = parse(Language::Go, GIN_SRC);
    let records = extract(Language::Go, &repo_path("api/routes.go"), &tree, GIN_SRC.as_bytes(), &symbols, &[FrameworkId::Gin]);
    assert_eq!(records[1].enclosing, Some(0), "facts inside Register enclose to its symbol");
    // Other frameworks' extractors do not claim gin routers.
    assert_eq!(facts(Language::Go, "api/routes.go", GIN_SRC, FrameworkId::Echo), vec![]);
}

const CHI_SRC: &str = r#"package web

func NewRouter(h *Handlers) http.Handler {
	r := chi.NewRouter()
	r.Use(middleware.Logger)
	r.Route("/api", func(r chi.Router) {
		r.With(auth).Get("/users/{id}", h.GetUser)
		r.Route("/admin", func(ar chi.Router) {
			ar.Post("/ban", h.Ban)
		})
		r.Group(func(r chi.Router) {
			r.Delete("/users/{id}", h.DeleteUser)
		})
	})
	r.Mount("/debug", debugRouter())
	r.Mount("/static", files)
	r.Method("GET", "/health", healthHandler)
	r.HandleFunc("/ws", h.WS)
	return r
}

func debugRouter() chi.Router {
	r := chi.NewRouter()
	r.Get("/vars", expvar.Handler().ServeHTTP)
	return r
}
"#;

#[test]
fn app_map_chi_nested_route_closures_and_mount() {
    let got = facts(Language::Go, "web/router.go", CHI_SRC, FrameworkId::Chi);
    let want = vec![
        (4, group("r", "", "NewRouter()")),
        (6, group("r@6", "/api", "r")),
        (7, decl(Some("GET"), path("/users/{id}"), Some("r@6"), named("GetUser", Some("h")), &["auth"])),
        (8, group("ar@8", "/admin", "r@6")),
        (9, decl(Some("POST"), path("/ban"), Some("ar@8"), named("Ban", Some("h")), &[])),
        (11, group("r@11", "", "r@6")),
        (12, decl(Some("DELETE"), path("/users/{id}"), Some("r@11"), named("DeleteUser", Some("h")), &[])),
        (15, group("debugRouter()", "/debug", "r")),
        (16, group("files", "/static", "r")),
        (17, decl(Some("GET"), path("/health"), Some("r"), named("healthHandler", None), &[])),
        (18, decl(None, path("/ws"), Some("r"), named("WS", Some("h")), &[])),
        (23, group("r", "", "debugRouter()")),
        (24, decl(Some("GET"), path("/vars"), Some("r"), named("ServeHTTP", Some("expvar.Handler()")), &[])),
    ];
    assert_eq!(got, want);
}

const GORILLA_SRC: &str = r#"package main

func routes(r *mux.Router, h *H) {
	r.HandleFunc("/items", h.List).Methods("GET", "HEAD")
	r.HandleFunc("/items/{id}", h.Get)
	api := r.PathPrefix("/api").Subrouter()
	api.Handle("/users", usersHandler).Methods(http.MethodPost, "PUT")
	r.PathPrefix("/static").Handler(http.FileServer(dir))
}
"#;

#[test]
fn app_map_gorilla_subrouter_and_methods() {
    let got = facts(Language::Go, "main.go", GORILLA_SRC, FrameworkId::GorillaMux);
    let want = vec![
        (3, group("r", "", "routes()")),
        (4, decl(Some("GET"), path("/items"), Some("r"), named("List", Some("h")), &[])),
        (4, decl(Some("HEAD"), path("/items"), Some("r"), named("List", Some("h")), &[])),
        (5, decl(None, path("/items/{id}"), Some("r"), named("Get", Some("h")), &[])),
        (6, group("api", "/api", "r")),
        (7, decl(Some("PUT"), path("/users"), Some("api"), named("usersHandler", None), &[])),
        (8, decl(None, path("/static"), Some("r"), named("FileServer", Some("http")), &[])),
    ];
    assert_eq!(got, want);
}

const ECHO_SRC: &str = r#"package main

func main() {
	e := echo.New()
	g := e.Group("/admin", middleware.BasicAuth(check))
	g.GET("/users/:id", getUser, audit)
	e.POST("/login", login)
	e.Match([]string{"GET", "POST"}, "/search", search)
	e.Any("/hook", hook)
}
"#;

#[test]
fn app_map_echo_group() {
    let got = facts(Language::Go, "main.go", ECHO_SRC, FrameworkId::Echo);
    let want = vec![
        (4, group("e", "", "main()")),
        (5, group("g", "/admin", "e")),
        (6, decl(Some("GET"), path("/users/:id"), Some("g"), named("getUser", None), &["audit"])),
        (7, decl(Some("POST"), path("/login"), Some("e"), named("login", None), &[])),
        (8, decl(Some("GET"), path("/search"), Some("e"), named("search", None), &[])),
        (8, decl(Some("POST"), path("/search"), Some("e"), named("search", None), &[])),
        (9, decl(None, path("/hook"), Some("e"), named("hook", None), &[])),
    ];
    assert_eq!(got, want);
}

const NET_HTTP_SRC: &str = r#"package main

func main() {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /items/{id}", getItem)
	mux.Handle("POST example.com/items", http.HandlerFunc(createItem))
	http.HandleFunc("/legacy", legacy)
	http.Handle(fmt.Sprintf("/v%d/status", version), status)
	http.ListenAndServe(":8080", mux)
}
"#;

#[test]
fn app_map_net_http_method_patterns() {
    let got = facts(Language::Go, "main.go", NET_HTTP_SRC, FrameworkId::GoNetHttp);
    let want = vec![
        (4, group("mux", "", "main()")),
        (5, decl(Some("GET"), path("/items/{id}"), Some("mux"), named("getItem", None), &[])),
        (6, decl(Some("POST"), path("/items"), Some("mux"), named("createItem", None), &[])),
        (7, decl(None, path("/legacy"), None, named("legacy", None), &[])),
        (8, decl(None, tpl(vec![lit("/v"), var("version"), lit("/status")]), None, named("status", None), &[])),
    ];
    assert_eq!(got, want);
}

// ---------------------------------------------------------------------------------------------
// JavaScript / TypeScript

#[test]
fn app_map_fetch_with_method() {
    let src = r#"export async function load(id: string) {
  await fetch(`/api/items/${id}?full=1`, { method: "post" });
  await fetch(API + "/health");
  await fetch("/x", { method: verb });
}
"#;
    let got = facts(Language::TypeScript, "web/src/api.ts", src, FrameworkId::JsFetch);
    let want = vec![
        (2, call(Some("POST"), vec![lit("/api/items/"), var("id"), lit("?full=1")], "fetch")),
        (3, call(Some("GET"), vec![var("API"), lit("/health")], "fetch")),
        (4, call(None, vec![lit("/x")], "fetch")),
    ];
    assert_eq!(got, want);
}

#[test]
fn app_map_axios_create_base_url() {
    let src = r#"import axios from "axios";
const api = axios.create({ baseURL: "/api/v1" });
const ext = axios.create({ baseURL: process.env.BASE });
export const list = () => api.get("/connections");
export const del = (id) => ext.delete(`/c/${id}`);
axios.post("/login", body);
axios({ url: "/raw", method: "put" });
api.request({ url: "/r" });
axios.get("https://x.io/a");
"#;
    let got = facts(Language::JavaScript, "web/src/api.js", src, FrameworkId::Axios);
    let want = vec![
        (4, call(Some("GET"), vec![lit("/api/v1/connections")], "api")),
        (5, call(Some("DELETE"), vec![var("baseURL"), lit("/c/"), var("id")], "ext")),
        (6, call(Some("POST"), vec![lit("/login")], "axios")),
        (7, call(Some("PUT"), vec![lit("/raw")], "axios")),
        (8, call(Some("GET"), vec![lit("/api/v1/r")], "api")),
        (9, call(Some("GET"), vec![lit("https://x.io/a")], "axios")),
    ];
    assert_eq!(got, want);
}

#[test]
fn app_map_react_router_jsx_and_object_routes() {
    let src = r#"const router = createBrowserRouter([
  { path: "/", element: <Root />, children: [
    { index: true, Component: Home },
    { path: "projects/:id", element: <Project />, children: [
      { path: "settings", lazy: () => import("./s") },
    ] },
  ] },
]);
export function App() {
  return (
    <Routes>
      <Route path="/app" element={<Layout />}>
        <Route index element={<Dashboard />} />
        <Route path="users/:id" element={<Users.Detail />} />
        <Route path="/abs" component={Abs} />
        <Route element={<Guard />}>
          <Route path="billing" element={<Billing />} />
        </Route>
      </Route>
    </Routes>
  );
}
"#;
    let got = facts(Language::Tsx, "web/src/App.tsx", src, FrameworkId::ReactRouter);
    let want = vec![
        (2, page("/", "Root")),
        (3, page("/", "Home")),
        (4, page("/projects/:id", "Project")),
        (12, page("/app", "Layout")),
        (13, page("/app", "Dashboard")),
        (14, page("/app/users/:id", "Users.Detail")),
        (15, page("/abs", "Abs")),
        (17, page("/app/billing", "Billing")),
    ];
    assert_eq!(got, want);
}

#[test]
fn app_map_next_pages_app_and_api_paths() {
    let cases: Vec<(&str, &str, Facts)> = vec![
        ("web/src/pages/blog/[slug].tsx", "export default function BlogPost() {}\n", vec![(1, page("/blog/[slug]", "BlogPost"))]),
        ("web/src/pages/index.tsx", "const Home = () => null;\nexport default Home;\n", vec![(2, page("/", "Home"))]),
        (
            "web/src/pages/api/users/[id].ts",
            "export default async function handler(req, res) {}\n",
            vec![(1, decl(None, path("/api/users/[id]"), None, named("handler", None), &[]))],
        ),
        ("web/src/pages/_app.tsx", "export default function App() {}\n", vec![]),
        (
            "web/app/(shop)/products/[...slug]/page.tsx",
            "export default function ProductPage() {}\n",
            vec![(1, page("/products/[...slug]", "ProductPage"))],
        ),
        (
            "web/app/api/items/route.ts",
            "import { NextResponse } from \"next/server\";\nexport async function GET() {}\nexport const POST = async () => {};\nconst handler = () => {};\nexport { handler as DELETE };\n",
            vec![
                (2, decl(Some("GET"), path("/api/items"), None, named("GET", None), &[])),
                (3, decl(Some("POST"), path("/api/items"), None, named("POST", None), &[])),
                (5, decl(Some("DELETE"), path("/api/items"), None, named("DELETE", None), &[])),
            ],
        ),
        ("web/app/layout.tsx", "export default function Layout() {}\n", vec![]),
    ];
    for (file, src, want) in cases {
        let lang = if file.ends_with(".tsx") { Language::Tsx } else { Language::TypeScript };
        assert_eq!(facts(lang, file, src, FrameworkId::NextRoutes), want, "{file}");
    }
}

// ---------------------------------------------------------------------------------------------
// Clojure / ClojureScript

#[test]
fn app_map_cljs_http_xhrio_map_with_str() {
    let src = r#"(ns app.events (:require [re-frame.core :as rf]))
(rf/reg-event-fx ::fetch
  (fn [_ [_ id]]
    {:http-xhrio {:method :get
                  :uri (str config/api-base "/connections/" id)
                  :on-success [::ok]}}))
(def req {:url "/plain" :method "post"})
(def other {:uri some-var})
"#;
    let got = facts(Language::ClojureScript, "src/app/events.cljs", src, FrameworkId::CljsHttpMaps);
    let want = vec![
        (4, call(Some("GET"), vec![var("config/api-base"), lit("/connections/"), var("id")], ":http-xhrio")),
        (7, call(Some("POST"), vec![lit("/plain")], "map")),
    ];
    assert_eq!(got, want);
    assert_eq!(facts(Language::ClojureScript, "src/app/events.cljs", src, FrameworkId::ReFrame), vec![]);
}

#[test]
fn app_map_reitit_nested_routes() {
    let src = r#"(def routes
  ["/api" {:middleware [wrap-auth]}
   ["/users" {:get {:handler users/list-users}
              :post users/create-user}]
   ["/health" {:get (fn [_] {:status 200})}]
   [["/a" {:handler a/h}] ["/b" {:view b-page}]]])
(def front ["" ["/" {:name ::home :view home/page}]])
"#;
    let symbols = [func(0, "routes", 1, 6)];
    let got = facts_at(Language::Clojure, "src/app/routes.clj", src, FrameworkId::Reitit, &symbols);
    let want = vec![
        (3, decl(Some("GET"), path("/api/users"), None, named("list-users", Some("users")), &["wrap-auth"])),
        (3, decl(Some("POST"), path("/api/users"), None, named("create-user", Some("users")), &["wrap-auth"])),
        (5, decl(Some("GET"), path("/api/health"), None, HandlerRef::Inline { enclosing: Some(0), line: 5 }, &["wrap-auth"])),
        (6, decl(None, path("/api/a"), None, named("h", Some("a")), &["wrap-auth"])),
        (6, page("/api/b", "b-page")),
        (7, page("/", "home/page")),
    ];
    assert_eq!(got, want);
}

// ---------------------------------------------------------------------------------------------
// SQL

#[test]
fn app_map_sql_strings_and_gorm() {
    let src = r#"package db
const q = `SELECT id FROM "public"."users" u JOIN orders o ON o.uid = u.id`
func f() {
	db.Exec("INSERT INTO audit_log (a) VALUES ($1)")
	db.Exec("DELETE FROM sessions WHERE id = $1")
	db.Exec("update Accounts set x = 1")
	errors.New("error from server")
	db.Table("legacy_items").Find(&x)
	db.Model(&UserRole{}).Update("x", 1)
	db.Exec("SELECT 1 FROM t; SELECT 2 FROM t")
}
"#;
    let got = facts(Language::Go, "db/q.go", src, FrameworkId::SqlStrings);
    let want = vec![
        (2, table("public.users", TableOp::Select)),
        (2, table("orders", TableOp::Select)),
        (4, table("audit_log", TableOp::Insert)),
        (5, table("sessions", TableOp::Delete)),
        (6, table("accounts", TableOp::Update)),
        (8, table("legacy_items", TableOp::Unknown)),
        (9, table("user_roles", TableOp::Unknown)),
        (10, table("t", TableOp::Select)),
    ];
    assert_eq!(got, want);

    let ts = "const rows = await db.query(`SELECT * FROM widgets WHERE id = ${id}`);\nthrow new Error(\"error from server\");\n";
    assert_eq!(facts(Language::TypeScript, "web/db.ts", ts, FrameworkId::SqlStrings), vec![(1, table("widgets", TableOp::Select))]);

    let py = "rows = conn.execute(f\"select * from {schema}.x join items i on true\")\n";
    assert_eq!(facts(Language::Python, "app/db.py", py, FrameworkId::SqlStrings), vec![(1, table("items", TableOp::Select))]);
}

// ---------------------------------------------------------------------------------------------
// Detection

#[test]
fn app_map_detect_frameworks_root_and_subdirs() {
    let manifests = vec![
        (
            repo_path("go.mod"),
            b"module example.com/x\n\ngo 1.22\n\nrequire (\n\tgithub.com/gin-gonic/gin v1.9.1\n\tgithub.com/go-chi/chi/v5 v5.0.12 // indirect\n)\n\nrequire github.com/gorilla/muxer v1.0.0\n".to_vec(),
        ),
        (
            repo_path("web/package.json"),
            br#"{"dependencies": {"axios": "^1.6.0", "react-router-dom": "^6"}, "devDependencies": {"next": "14.0.0"}}"#.to_vec(),
        ),
        (
            repo_path("ui/deps.edn"),
            b"{:deps {re-frame/re-frame {:mvn/version \"1.4\"} metosin/reitit {:mvn/version \"0.7\"} org.clojure/clojurescript {:mvn/version \"1.11\"}}}".to_vec(),
        ),
        (repo_path("ui/README.md"), b"go.mod package.json".to_vec()),
        (repo_path("tools/requirements.txt"), b"requests\n".to_vec()),
    ];
    let scope = |root: Option<&str>, framework| FrameworkScope { root: root.map(repo_path), framework };
    let want = vec![
        scope(None, FrameworkId::GoNetHttp),
        scope(None, FrameworkId::Gin),
        scope(None, FrameworkId::Chi),
        scope(None, FrameworkId::SqlStrings),
        scope(Some("tools"), FrameworkId::SqlStrings),
        scope(Some("ui"), FrameworkId::ReFrame),
        scope(Some("ui"), FrameworkId::CljsHttpMaps),
        scope(Some("ui"), FrameworkId::Reitit),
        scope(Some("ui"), FrameworkId::SqlStrings),
        scope(Some("web"), FrameworkId::JsFetch),
        scope(Some("web"), FrameworkId::Axios),
        scope(Some("web"), FrameworkId::ReactRouter),
        scope(Some("web"), FrameworkId::NextRoutes),
        scope(Some("web"), FrameworkId::SqlStrings),
    ];
    assert_eq!(detect_frameworks(&manifests), want);
}

#[test]
fn app_map_extract_skips_frameworks_and_languages_not_requested() {
    let tree = parse(Language::Go, GIN_SRC);
    let path = repo_path("api/routes.go");
    assert!(extract(Language::Go, &path, &tree, GIN_SRC.as_bytes(), &[], &[FrameworkId::Axios]).is_empty());
    assert!(extract(Language::Python, &path, &tree, GIN_SRC.as_bytes(), &[], &[FrameworkId::Gin]).is_empty());
}
