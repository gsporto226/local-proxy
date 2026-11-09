// Command lpref writes CLIProxyAPI's Gemini translations of the fixtures in
// tests/fixtures/gemini as golden files. It imports CLIProxyAPI internal
// packages, so generate.sh copies it into a checkout; it only builds there.
package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	claude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/gemini/claude"
)

func main() {
	dir := os.Args[1]
	requests, _ := filepath.Glob(filepath.Join(dir, "*.anthropic.json"))
	for _, path := range requests {
		in, err := os.ReadFile(path)
		check(err)
		out, err := claude.ConvertClaudeRequestToGemini("gemini-2.5-pro", in, true)
		check(err)
		write(strings.TrimSuffix(path, ".anthropic.json")+".reference-request.json", out)
	}
	responses, _ := filepath.Glob(filepath.Join(dir, "*.gemini.json"))
	for _, path := range responses {
		in, err := os.ReadFile(path)
		check(err)
		out := claude.ConvertGeminiResponseToClaudeNonStream(context.Background(), "", []byte(`{}`), nil, in, nil)
		write(strings.TrimSuffix(path, ".gemini.json")+".reference-response.json", out)
	}
}

func write(path string, data []byte) {
	check(os.WriteFile(path, data, 0o644))
	fmt.Println("wrote", path)
}

func check(err error) {
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
