// Example program (kind = "command") in Go: an ordinary net/http server built
// by TinyGo for wasip2. Networking goes through the hand-written wasinet driver (see its description).
package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"sync/atomic"

	_ "wshell.example/hello-go/wasinet"
)

const port = 8482

var requests atomic.Uint64

func main() {
	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		n := requests.Add(1)
		fmt.Printf("%s %s %s\n", r.RemoteAddr, r.Method, r.URL)
		w.Header().Set("content-type", "application/json")
		json.NewEncoder(w).Encode(map[string]any{
			"hello":    "from go",
			"path":     r.URL.Path,
			"requests": n,
		})
	})

	srv := &http.Server{Addr: fmt.Sprintf(":%d", port), Handler: mux}
	fmt.Printf("hello-go: listening on port %d\n", port)
	if err := srv.ListenAndServe(); err != nil {
		fmt.Println("hello-go:", err)
	}
}
