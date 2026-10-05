package main

import (
	"errors"
	"fmt"
	"log"
	"maps"
	"math"
	"slices"
	"strconv"
	"strings"

	"github.com/hashicorp/hcl/v2"
	"github.com/hashicorp/hcl/v2/hclsyntax"
	"github.com/zclconf/go-cty/cty"
	"github.com/zclconf/go-cty/cty/convert"
)

// body is the form of the values HCL reads from root, a body with only data. The
// README tells the form. It stops the program at a node it cannot print.
func body(name string, src []byte, root *hclsyntax.Body) string {
	var items []string
	for _, key := range slices.Sorted(maps.Keys(root.Attributes)) {
		val := value(name, src, root.Attributes[key].Expr)
		items = append(items, quote(key)+" = "+val)
	}
	for _, block := range root.Blocks {
		words := []string{quote(block.Type)}
		for _, label := range block.Labels {
			words = append(words, quote(label))
		}
		words = append(words, body(name, src, block.Body))
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
			text := key.AsString()
			items[text] = quote(text) + " = " + value(name, src, item.ValueExpr)
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
		return "$" + strings.Join(steps, ".")
	case *hclsyntax.FunctionCallExpr:
		var args []string
		for _, arg := range expr.Args {
			args = append(args, value(name, src, arg))
		}
		return quote(expr.Name) + "(" + strings.Join(args, ", ") + ")"
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

// number is the exact integer when the number is written with digits only.
// Otherwise it is the bits of the float64 nearest to the written number, with no
// sign on zero, such as `f3ff8000000000000` for 1.5.
func number(src []byte, literal *hclsyntax.LiteralValueExpr, negative bool) string {
	if i, integer := digits(src, literal); integer {
		if negative {
			i.Neg(i)
		}
		return i.String()
	}
	// HCL holds a 512-bit value, and rounding it again to a float64 can miss the
	// nearest float64.
	text := string(literal.Range().SliceBytes(src))
	f, err := strconv.ParseFloat(scientific(text), 64)
	if err != nil && !errors.Is(err, strconv.ErrRange) {
		log.Fatalf("%s", err)
	}
	if negative {
		f = -f
	}
	if f == 0 {
		f = math.Abs(f)
	}
	return fmt.Sprintf("f%016x", math.Float64bits(f))
}

// scientific is the float that text writes, as `d.ddd` with no leading zero and an
// exponent in -400..400. strconv.ParseFloat stops reading the digits of a long
// exponent, and past 400 each float64 is infinite or zero.
func scientific(text string) string {
	mantissa, written, found := strings.Cut(strings.ToLower(text), "e")
	if !found {
		written = "0"
	}
	exponent, err := strconv.ParseInt(written, 10, 64)
	if err != nil {
		log.Fatalf("%s", err)
	}
	whole, fraction, _ := strings.Cut(mantissa, ".")
	digits := whole + fraction
	significant := strings.TrimLeft(digits, "0")
	if significant == "" {
		return "0"
	}
	zeros := len(digits) - len(significant)
	place := max(-1<<62, min(exponent, 1<<62)) + int64(len(whole)-zeros-1)
	place = max(-400, min(place, 400))
	return fmt.Sprintf("%s.%se%d", significant[:1], significant[1:], place)
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
