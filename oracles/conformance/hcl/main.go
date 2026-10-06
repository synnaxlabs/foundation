// Command verdicts writes verdicts.txt: the verdict of the pinned HCL version on each
// text in texts/, and the codes of the forms outside data in each accepted text. It
// also writes values.txt: the values HCL reads from each text with only data. Run it
// in this directory with `go run .`.
package main

import (
	"fmt"
	"log"
	"maps"
	"math/big"
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
	var verdicts, values strings.Builder
	for _, out := range []*strings.Builder{&verdicts, &values} {
		fmt.Fprintf(out, "# %s %s. Made by main.go; do not edit.\n", module, version())
	}
	for _, path := range paths {
		src, err := os.ReadFile(path)
		if err != nil {
			log.Fatal(err)
		}
		name := strings.TrimSuffix(filepath.Base(path), ".hcl")
		file, diags := hclsyntax.ParseConfig(src, name, hcl.InitialPos)
		if diags.HasErrors() {
			fmt.Fprintf(&verdicts, "%s refused\n", name)
			continue
		}
		root := file.Body.(*hclsyntax.Body)
		found := codes(name, src, root)
		verdict := strings.Join(slices.Concat([]string{"accepted"}, found), " ")
		fmt.Fprintf(&verdicts, "%s %s\n", name, verdict)
		if len(found) == 0 {
			fmt.Fprintf(&values, "%s %s\n", name, body(name, src, root))
		}
	}
	write("verdicts.txt", verdicts.String())
	write("values.txt", values.String())
}

func write(path, text string) {
	if err := os.WriteFile(path, []byte(text), 0o644); err != nil {
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

// codes are the sorted codes of the forms outside data in body.
func codes(name string, src []byte, body *hclsyntax.Body) []string {
	codes := map[string]bool{}
	visit := func(node hclsyntax.Node) hcl.Diagnostics {
		for _, code := range forms(name, src, node) {
			codes[code] = true
		}
		return nil
	}
	hclsyntax.VisitAll(body, visit)
	return slices.Sorted(maps.Keys(codes))
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
			// Text between interpolations is a string literal; `${"x"}` is a template.
			literal, ok := part.(*hclsyntax.LiteralValueExpr)
			if !ok || literal.Val.Type() != cty.String {
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
	case *hclsyntax.IndexExpr:
		return index(node.Collection)
	case *hclsyntax.RelativeTraversalExpr:
		return index(node.Source)
	case *hclsyntax.ScopeTraversalExpr:
		// A string index is one more segment of the name.
		for _, step := range node.Traversal {
			index, ok := step.(hcl.TraverseIndex)
			if ok && index.Key.Type() != cty.String {
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
// without a `-`: none, or the code of rounded. The walk finds the forms inside the
// key. Any other key is an expression, which is not known.
func key(src []byte, node *hclsyntax.ObjectConsKeyExpr) (codes []string, known bool) {
	switch wrapped := node.Wrapped.(type) {
	case *hclsyntax.ParenthesesExpr, *hclsyntax.TemplateExpr,
		*hclsyntax.TemplateWrapExpr:
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

// index is hcl.index for an index into source, or none when source is the item of a
// splat, whose index is part of the splat.
func index(source hclsyntax.Expression) []string {
	if _, item := source.(*hclsyntax.AnonSymbolExpr); item {
		return nil
	}
	return []string{"hcl.index"}
}

// rounded is hcl.number-key for a number key with a fraction or an exponent, and for
// an integer key that HCL rounds when it makes the key.
func rounded(src []byte, literal *hclsyntax.LiteralValueExpr) []string {
	if literal.Val.IsNull() || literal.Val.Type() != cty.Number {
		return nil
	}
	written, integer := digits(src, literal)
	if !integer {
		return []string{"hcl.number-key"}
	}
	if read, _ := literal.Val.AsBigFloat().Int(nil); read.Cmp(written) != 0 {
		return []string{"hcl.number-key"}
	}
	return nil
}

// digits is the integer that literal is written as, when it is written with digits
// only. A Document holds such a number as an integer, and any other as a float.
func digits(src []byte, literal *hclsyntax.LiteralValueExpr) (*big.Int, bool) {
	return new(big.Int).SetString(string(literal.Range().SliceBytes(src)), 10)
}
