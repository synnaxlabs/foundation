// Command verdicts writes verdicts.txt: the verdict of the pinned HCL version on each
// text in texts/, and the codes of the forms outside data in each accepted text. Run
// it in this directory with `go run .`.
package main

import (
	"fmt"
	"log"
	"os"
	"path/filepath"
	"runtime/debug"
	"slices"
	"strings"

	"github.com/hashicorp/hcl/v2"
	"github.com/hashicorp/hcl/v2/hclsyntax"
	"github.com/zclconf/go-cty/cty"
)

const module = "github.com/hashicorp/hcl/v2"

func main() {
	paths, err := filepath.Glob(filepath.Join("texts", "*.hcl"))
	if err != nil {
		log.Fatal(err)
	}
	var out strings.Builder
	fmt.Fprintf(&out, "# %s %s. Made by main.go; do not edit.\n", module, version())
	for _, path := range paths {
		src, err := os.ReadFile(path)
		if err != nil {
			log.Fatal(err)
		}
		name := strings.TrimSuffix(filepath.Base(path), ".hcl")
		fmt.Fprintf(&out, "%s %s\n", name, verdict(name, src))
	}
	if err := os.WriteFile("verdicts.txt", []byte(out.String()), 0o644); err != nil {
		log.Fatal(err)
	}
}

// version is the HCL version this program is built with.
func version() string {
	info, ok := debug.ReadBuildInfo()
	if !ok {
		log.Fatal("the program has no build info")
	}
	for _, dep := range info.Deps {
		if dep.Path == module {
			return dep.Version
		}
	}
	log.Fatalf("the build info has no %s", module)
	return ""
}

// verdict is "refused", "accepted", or "accepted" and the sorted codes of the forms
// outside data.
func verdict(name string, src []byte) string {
	file, diags := hclsyntax.ParseConfig(src, name, hcl.InitialPos)
	if diags.HasErrors() {
		return "refused"
	}
	codes := map[string]bool{}
	visit := func(node hclsyntax.Node) hcl.Diagnostics {
		for _, code := range forms(name, src, node) {
			codes[code] = true
		}
		return nil
	}
	hclsyntax.VisitAll(file.Body.(*hclsyntax.Body), visit)
	if len(codes) == 0 {
		return "accepted"
	}
	sorted := make([]string, 0, len(codes))
	for code := range codes {
		sorted = append(sorted, code)
	}
	slices.Sort(sorted)
	return "accepted " + strings.Join(sorted, " ")
}

// forms are the diagnostic codes of the forms outside data that node shows, or none
// for a node of data. It stops the program at a node it does not know.
func forms(name string, src []byte, node hclsyntax.Node) []string {
	switch node := node.(type) {
	case *hclsyntax.Body, hclsyntax.Attributes, *hclsyntax.Attribute,
		hclsyntax.Blocks, *hclsyntax.Block, hclsyntax.ChildScope,
		*hclsyntax.TupleConsExpr, *hclsyntax.ObjectConsExpr, *hclsyntax.AnonSymbolExpr:
		return nil
	case *hclsyntax.LiteralValueExpr:
		if node.Val.IsNull() {
			return []string{"hcl.null"}
		}
		return nil
	case *hclsyntax.TemplateExpr:
		for _, part := range node.Parts {
			if _, literal := part.(*hclsyntax.LiteralValueExpr); !literal {
				return []string{"hcl.template"}
			}
		}
		return nil
	case *hclsyntax.TemplateWrapExpr, *hclsyntax.TemplateJoinExpr:
		return []string{"hcl.template"}
	case *hclsyntax.BinaryOpExpr:
		return []string{"hcl.operator"}
	case *hclsyntax.UnaryOpExpr:
		if literal, ok := node.Val.(*hclsyntax.LiteralValueExpr); ok &&
			node.Op == hclsyntax.OpNegate && literal.Val.Type() == cty.Number {
			return nil
		}
		return []string{"hcl.operator"}
	case *hclsyntax.ConditionalExpr:
		return []string{"hcl.conditional"}
	case *hclsyntax.ForExpr:
		return []string{"hcl.for"}
	case *hclsyntax.IndexExpr, *hclsyntax.RelativeTraversalExpr:
		return []string{"hcl.index"}
	case *hclsyntax.ScopeTraversalExpr:
		for _, step := range node.Traversal {
			if _, index := step.(hcl.TraverseIndex); index {
				return []string{"hcl.index"}
			}
		}
		return nil
	case *hclsyntax.SplatExpr:
		return []string{"hcl.splat"}
	case *hclsyntax.ParenthesesExpr:
		return []string{"hcl.parentheses"}
	case *hclsyntax.FunctionCallExpr:
		var codes []string
		if strings.Contains(node.Name, "::") {
			codes = append(codes, "hcl.namespace")
		}
		if node.ExpandFinal {
			codes = append(codes, "hcl.expansion")
		}
		return codes
	case *hclsyntax.ObjectConsKeyExpr:
		if codes, known := key(src, node); known {
			return codes
		}
	}
	log.Fatalf("%s: %T at %s is not in the table of forms", name, node, node.Range())
	return nil
}

// key is the code for an object key that is a name, a string, or a number with or
// without a `-`: none, or hcl.number-key for a number that HCL rounds. The walk finds
// the forms inside the key. Any other key is an expression, which is not known.
func key(src []byte, node *hclsyntax.ObjectConsKeyExpr) (codes []string, known bool) {
	switch wrapped := node.Wrapped.(type) {
	case *hclsyntax.ParenthesesExpr, *hclsyntax.TemplateExpr, *hclsyntax.TemplateWrapExpr:
		return nil, true
	case *hclsyntax.ScopeTraversalExpr:
		return nil, len(wrapped.Traversal) == 1
	case *hclsyntax.LiteralValueExpr:
		return rounded(src, wrapped), true
	case *hclsyntax.UnaryOpExpr:
		if literal, ok := wrapped.Val.(*hclsyntax.LiteralValueExpr); ok &&
			wrapped.Op == hclsyntax.OpNegate && literal.Val.Type() == cty.Number {
			return rounded(src, literal), true
		}
	}
	return nil, false
}

// rounded is hcl.number-key for a number with a fraction, an exponent, or more than
// 154 digits, which HCL rounds when it makes the key.
func rounded(src []byte, literal *hclsyntax.LiteralValueExpr) []string {
	if literal.Val.IsNull() || literal.Val.Type() != cty.Number {
		return nil
	}
	text := string(literal.Range().SliceBytes(src))
	if strings.ContainsAny(text, ".eE") || len(text) > 154 {
		return []string{"hcl.number-key"}
	}
	return nil
}
