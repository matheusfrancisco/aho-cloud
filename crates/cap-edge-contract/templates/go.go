// Package principal verifies the aho cloud principal for project {{PROJECT}} ({{STAGE}}).
// The edge authenticates users and forwards a signed principal; this package only verifies it.
// Authorize in your handlers by (Claims.Issuer, Claims.Subject). Never add a login flow here.
//
//	go get github.com/golang-jwt/jwt/v5 github.com/MicahParks/keyfunc/v3
package principal

import (
	"context"
	"errors"
	"net/http"
	"time"

	"github.com/MicahParks/keyfunc/v3"
	"github.com/golang-jwt/jwt/v5"
)

const (
	Issuer          = "{{ISSUER}}"
	Audience        = "{{AUDIENCE}}"
	JWKSURL         = "{{JWKS_URL}}"
	Header          = "{{HEADER}}"
	MaxLifetimeSecs = {{TTL}}
	LeewaySecs      = {{LEEWAY}}
)

// Claims are the verified principal.
type Claims struct {
	Email         string   `json:"email,omitempty"`
	EmailVerified bool     `json:"email_verified"`
	Groups        []string `json:"groups"`
	AMR           []string `json:"amr"`
	SID           string   `json:"sid"`
	jwt.RegisteredClaims
}

type ctxKey struct{}

// Verifier holds the project's JWKS, refreshed in the background.
type Verifier struct{ keys keyfunc.Keyfunc }

// New fetches the JWKS; ctx bounds the background refresh.
func New(ctx context.Context) (*Verifier, error) {
	k, err := keyfunc.NewDefaultCtx(ctx, []string{JWKSURL})
	if err != nil {
		return nil, err
	}
	return &Verifier{keys: k}, nil
}

// Verify checks the signature and every claim.
func (v *Verifier) Verify(token string) (*Claims, error) {
	claims := &Claims{}
	_, err := jwt.ParseWithClaims(token, claims, v.keys.Keyfunc,
		jwt.WithValidMethods([]string{"ES256"}),
		jwt.WithIssuer(Issuer),
		jwt.WithAudience(Audience),
		jwt.WithLeeway(LeewaySecs*time.Second),
		jwt.WithIssuedAt(),
		jwt.WithExpirationRequired(),
	)
	if err != nil {
		return nil, err
	}
	if claims.Subject == "" || claims.SID == "" || claims.IssuedAt == nil {
		return nil, errors.New("principal token is missing sub, sid or iat")
	}
	if claims.ExpiresAt.Sub(claims.IssuedAt.Time) > MaxLifetimeSecs*time.Second {
		return nil, errors.New("principal token lives too long")
	}
	return claims, nil
}

// Require answers 401 unless a valid principal is attached: routes in `edge` mode.
func (v *Verifier) Require(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		claims, err := v.Verify(r.Header.Get(Header))
		if err != nil {
			http.Error(w, "invalid principal", http.StatusUnauthorized)
			return
		}
		next.ServeHTTP(w, r.WithContext(context.WithValue(r.Context(), ctxKey{}, claims)))
	})
}

// Optional attaches the principal when someone is signed in: routes in `optional` mode.
func (v *Verifier) Optional(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get(Header) == "" {
			next.ServeHTTP(w, r)
			return
		}
		v.Require(next).ServeHTTP(w, r)
	})
}

// From returns the verified principal of a request, if any.
func From(r *http.Request) (*Claims, bool) {
	c, ok := r.Context().Value(ctxKey{}).(*Claims)
	return c, ok
}
