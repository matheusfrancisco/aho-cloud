// Package api serves connections.
package api

import (
	"errors"
	"fmt"
	"net/http"

	"github.com/gin-gonic/gin"
)

// MaxRetries bounds retries.
const MaxRetries = 3

var defaultName = "conn"

// Server serves connections.
// It is safe for concurrent use.
type Server struct {
	Name string
}

type Store interface {
	Get(name string) (*Server, error)
}

// Connect opens the connection.
func (s *Server) Connect(name string) error {
	err := dial(name)
	if err != nil {
		return fmt.Errorf("failed fetching connection %s: %w", name, err)
	}
	return errors.New("connection refused by \"peer\"\n")
}

func handler(w http.ResponseWriter, r *http.Request) {
	http.Error(w, "session expired", http.StatusUnauthorized)
}

func getItem(c *gin.Context) {
	c.JSON(http.StatusNotFound, gin.H{"message": "item " + c.Param("id") + " not found"})
	log.Printf("request %s took %dms", c.Request.URL, 5)
	panic(fmt.Sprintf("unreachable state %d", 3))
}

func raw() string {
	return `raw multi
line`
}
