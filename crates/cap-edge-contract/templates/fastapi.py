# aho cloud principal verifier for FastAPI, for project {{PROJECT}} ({{STAGE}}).
# The edge authenticates users and forwards a signed principal; this dependency only verifies it.
# Authorize in your handlers by (principal["iss"], principal["sub"]). Never add a login flow here.
#
#   pip install "PyJWT[crypto]"
import jwt
from fastapi import HTTPException, Request

ISSUER = "{{ISSUER}}"
AUDIENCE = "{{AUDIENCE}}"
HEADER = "{{HEADER}}"
MAX_LIFETIME_SECS = {{TTL}}
LEEWAY_SECS = {{LEEWAY}}
_jwks = jwt.PyJWKClient("{{JWKS_URL}}", cache_keys=True)


def _verify(token: str) -> dict:
    key = _jwks.get_signing_key_from_jwt(token)
    claims = jwt.decode(
        token,
        key.key,
        algorithms=["ES256"],
        audience=AUDIENCE,
        issuer=ISSUER,
        leeway=LEEWAY_SECS,
        options={"require": ["iss", "aud", "sub", "iat", "exp", "sid"]},
    )
    if claims["exp"] - claims["iat"] > MAX_LIFETIME_SECS:
        raise jwt.InvalidTokenError("principal token lives too long")
    return claims


def principal(request: Request) -> dict:
    """The verified principal, for routes in `edge` mode; 401 without a valid one."""
    token = request.headers.get(HEADER)
    if not token:
        raise HTTPException(status_code=401, detail="no principal")
    try:
        return _verify(token)
    except (jwt.PyJWTError, jwt.PyJWKClientError):
        raise HTTPException(status_code=401, detail="invalid principal")


def optional_principal(request: Request) -> dict | None:
    """For routes in `optional` mode: None when nobody is signed in, 401 for an invalid token."""
    if not request.headers.get(HEADER):
        return None
    return principal(request)


# from fastapi import Depends
# @app.get("/me")
# def me(p: dict = Depends(principal)):
#     return {"iss": p["iss"], "sub": p["sub"]}
