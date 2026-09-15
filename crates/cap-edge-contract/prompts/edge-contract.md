## aho cloud: who the user is

Apps deployed on aho cloud never implement login. The edge in front of the app authenticates
people (password, magic link or OIDC, as the manifest's `auth` says) and forwards every request
with an `x-principal-jwt` header: a short-lived ES256 JWT signed with the project's keys, which are
published at `/.edge/jwks.json`. The app verifies that token and authorizes by the pair
`(iss, sub)`. It never stores passwords, sends login emails or talks to an identity provider.
`edge_verifier` writes the verification middleware for Express, FastAPI, Go or axum.

Declare how each path is gated in `edge.toml` at the workspace root. `edge` requires a login,
`optional` attaches the principal when someone is signed in, `bearer` takes the project service
token, `public` lets anyone in, and `passthrough` forwards untouched. The first matching route
wins, and `default` covers the rest. A webhook is a `public` route with
`verify = { kind = "stripe" | "github" | "hmac", secret = "<SECRET NAME>" }`, so the edge checks the
signature before the app sees the request. Check the routes with `edge_routes`.
