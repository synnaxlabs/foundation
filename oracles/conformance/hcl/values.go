package main

import (
	"fmt"
	"log"
	"maps"
	"math/big"
	"slices"
	"strconv"
	"strings"

	"github.com/hashicorp/hcl/v2"
	"github.com/hashicorp/hcl/v2/hclsyntax"
	"github.com/zclconf/go-cty/cty"
	"github.com/zclconf/go-cty/cty/convert"
)

// values is the form of the values HCL reads from body, a body with only data. The
// README tells the form. It stops the program at a node it cannot print.
func values(name string, src []byte, body *hclsyntax.Body) string {
	var items []string
	for _, key := range slices.Sorted(maps.Keys(body.Attributes)) {
		items = append(items, quote(key)+" = "+value(name, src, body.Attributes[key].Expr))
	}
	for _, block := range body.Blocks {
		words := []string{block.Type}
		for _, label := range block.Labels {
			words = append(words, quote(label))
		}
		words = append(words, values(name, src, block.Body))
		items = append(items, strings.Join(words, " "))
	}
	return "{" + strings.Join(items, ", ") + "}"
}

func value(name string, src []byte, expr hclsyntax.Expression) string {
	switch expr := expr.(type) {
	case *hclsyntax.LiteralValueExpr:
		if expr.Val.Type() == cty.Bool {
			return strconv.FormatBool(expr.Val.True())
		}
		return number(src, expr, false)
	case *hclsyntax.UnaryOpExpr:
		return number(src, expr.Val.(*hclsyntax.LiteralValueExpr), true)
	case *hclsyntax.TemplateExpr:
		return quote(evaluate(name, expr).AsString())
	case *hclsyntax.TupleConsExpr:
		var items []string
		for _, item := range expr.Exprs {
			items = append(items, value(name, src, item))
		}
		return "[" + strings.Join(items, ", ") + "]"
	case *hclsyntax.ObjectConsExpr:
		// HCL makes each key a string, and a later item replaces an earlier one.
		items := map[string]string{}
		for _, item := range expr.Items {
			key, err := convert.Convert(evaluate(name, item.KeyExpr), cty.String)
			if err != nil {
				log.Fatalf("%s: %s", name, err)
			}
			items[key.AsString()] = quote(key.AsString()) + " = " + value(name, src, item.ValueExpr)
		}
		var sorted []string
		for _, key := range slices.Sorted(maps.Keys(items)) {
			sorted = append(sorted, items[key])
		}
		return "{" + strings.Join(sorted, ", ") + "}"
	case *hclsyntax.ScopeTraversalExpr:
		steps := []string{expr.Traversal.RootName()}
		for _, step := range expr.Traversal[1:] {
			steps = append(steps, step.(hcl.TraverseAttr).Name)
		}
		return strings.Join(steps, ".")
	case *hclsyntax.FunctionCallExpr:
		var args []string
		for _, arg := range expr.Args {
			args = append(args, value(name, src, arg))
		}
		return expr.Name + "(" + strings.Join(args, ", ") + ")"
	}
	log.Fatalf("%s: %T at %s has no form for its value", name, expr, expr.Range())
	return ""
}

func evaluate(name string, expr hclsyntax.Expression) cty.Value {
	val, diags := expr.Value(nil)
	if diags.HasErrors() {
		log.Fatalf("%s: %s", name, diags.Error())
	}
	return val
}

// number is the exact integer when the text has only digits, as a Document holds it.
// Otherwise it is the nearest float64, with no sign on zero, as `1.5e0`.
func number(src []byte, literal *hclsyntax.LiteralValueExpr, negative bool) string {
	val := new(big.Float).Copy(literal.Val.AsBigFloat())
	if negative {
		val.Neg(val)
	}
	if _, integer := new(big.Int).SetString(string(literal.Range().SliceBytes(src)), 10); integer {
		i, _ := val.Int(nil)
		return i.String()
	}
	f, _ := val.Float64()
	if f == 0 {
		f = 0
	}
	text := strconv.FormatFloat(f, 'e', -1, 64)
	mantissa, exponent, found := strings.Cut(text, "e")
	if !found {
		return text
	}
	e, _ := strconv.Atoi(exponent)
	return fmt.Sprintf("%se%d", mantissa, e)
}

// quote puts text in `"`, with `\` before `"` and `\`, and each character outside
// printable ASCII as `\u{hex}`.
func quote(text string) string {
	var out strings.Builder
	out.WriteByte('"')
	for _, r := range text {
		switch {
		case r == '"' || r == '\\':
			out.WriteByte('\\')
			out.WriteRune(r)
		case r >= ' ' && r <= '~':
			out.WriteRune(r)
		default:
			fmt.Fprintf(&out, "\\u{%x}", r)
		}
	}
	out.WriteByte('"')
	return out.String()
}
