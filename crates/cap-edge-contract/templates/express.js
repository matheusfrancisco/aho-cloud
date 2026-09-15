// aho cloud principal verifier for Express, for project {{PROJECT}} ({{STAGE}}).
// The edge authenticates users and forwards a signed principal; this middleware only verifies it.
// Authorize in your handlers by (req.principal.iss, req.principal.sub). Never add a login flow here.
//
//   npm install jose
import { createRemoteJWKSet, jwtVerify } from "jose";

const ISSUER = "{{ISSUER}}";
const AUDIENCE = "{{AUDIENCE}}";
const HEADER = "{{HEADER}}";
const MAX_LIFETIME_SECS = {{TTL}};
const LEEWAY_SECS = {{LEEWAY}};
const JWKS = createRemoteJWKSet(new URL("{{JWKS_URL}}"));

async function verify(token) {
  const { payload } = await jwtVerify(token, JWKS, {
    issuer: ISSUER,
    audience: AUDIENCE,
    algorithms: ["ES256"],
    clockTolerance: LEEWAY_SECS,
    requiredClaims: ["sub", "iat", "exp", "sid"],
  });
  const now = Math.floor(Date.now() / 1000);
  if (payload.iat > now + LEEWAY_SECS) throw new Error("principal token issued in the future");
  if (payload.exp - payload.iat > MAX_LIFETIME_SECS) throw new Error("principal token lives too long");
  return payload;
}

/**
 * Sets `req.principal` to the verified claims. `required: true` (routes in `edge` mode) answers
 * 401 without a valid principal; `required: false` (routes in `optional` mode) sets null when
 * nobody is signed in. A present but invalid token is always 401.
 */
export function principal({ required = true } = {}) {
  return async (req, res, next) => {
    const token = req.get(HEADER);
    if (!token) {
      if (required) return res.status(401).json({ error: "no principal" });
      req.principal = null;
      return next();
    }
    try {
      req.principal = await verify(token);
      return next();
    } catch {
      return res.status(401).json({ error: "invalid principal" });
    }
  };
}

// app.use("/app", principal());
// app.use("/", principal({ required: false }));
