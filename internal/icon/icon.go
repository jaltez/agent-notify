// Package icon renders the severity-colored disc used for the tray icon
// and toast logo. It generates bytes directly (32bpp DIB ICO for
// LoadImage, PNG elsewhere) with no image assets.
package icon

import (
	"bytes"
	"encoding/binary"
	"image"
	"image/color"
	"image/png"
	"math"
	"runtime"
)

// RGB is an 8-bit color channel triple.
type RGB struct{ R, G, B uint8 }

// severityColors maps the engine severity to brand colors: blocked red,
// offline amber, waiting-for-you green, working blue, idle gray.
var severityColors = map[string]RGB{
	"blocked": {0xE5, 0x48, 0x4D},
	"down":    {0xFF, 0xB0, 0x2E},
	"waiting": {0x22, 0xC5, 0x5E},
	"working": {0x3B, 0x82, 0xF6},
	"idle":    {0x6B, 0x72, 0x80},
}

// Color returns the color for a severity, defaulting to idle gray.
func Color(severity string) RGB {
	if c, ok := severityColors[severity]; ok {
		return c
	}
	return severityColors["idle"]
}

// pixelFn colors one pixel of a size×size icon: returns BGRA-premixed
// (R, G, B, alpha) for the given visual coordinates.
type pixelFn func(c RGB, size, x, y int) (uint8, uint8, uint8, uint8)

// Bytes returns tray icon bytes for a severity: a 32bpp ICO on Windows
// (LoadImage-compatible), a PNG elsewhere. Filled disc with a white halo.
func Bytes(severity string, size int) []byte {
	return render(circlePixel, Color(severity), size)
}

// BytesHollow returns the severity color as a ring over a white halo on a
// transparent background — the "off" phase of the attention blink.
func BytesHollow(severity string, size int) []byte {
	return render(ringPixel, Color(severity), size)
}

// render encodes a pixel function as an ICO (Windows) or PNG (elsewhere).
func render(pixel pixelFn, c RGB, size int) []byte {
	if runtime.GOOS == "windows" {
		return icoFrom(pixel, c, size)
	}
	return pngFrom(pixel, c, size)
}

// icoFrom wraps the pixel raster in an ICO container: ICONDIR + one
// ICONDIRENTRY + DIB (BITMAPINFOHEADER with doubled height, bottom-up
// BGRA rows, zero AND mask). 32bpp, alpha via the XOR mask.
func icoFrom(pixel pixelFn, c RGB, size int) []byte {
	pix := make([]byte, 0, size*size*4)
	for y := size - 1; y >= 0; y-- {
		for x := 0; x < size; x++ {
			r, g, b, a := pixel(c, size, x, y)
			pix = append(pix, b, g, r, a)
		}
	}
	maskRowBytes := ((size+7)/8 + 3) / 4 * 4
	mask := make([]byte, maskRowBytes*size) // fully transparent AND mask

	var buf bytes.Buffer
	binary.Write(&buf, binary.LittleEndian, uint16(0)) // reserved
	binary.Write(&buf, binary.LittleEndian, uint16(1)) // type: icon
	binary.Write(&buf, binary.LittleEndian, uint16(1)) // image count
	dim := byte(size)
	if size >= 256 {
		dim = 0
	}
	buf.WriteByte(dim)                                  // width
	buf.WriteByte(dim)                                  // height
	buf.WriteByte(0)                                    // palette colors
	buf.WriteByte(0)                                    // reserved
	binary.Write(&buf, binary.LittleEndian, uint16(1))  // planes
	binary.Write(&buf, binary.LittleEndian, uint16(32)) // bpp
	// dwBytesInRes covers the DIB header + pixel data + AND mask
	binary.Write(&buf, binary.LittleEndian, uint32(40+len(pix)+len(mask)))
	binary.Write(&buf, binary.LittleEndian, uint32(22)) // offset

	hdr := make([]byte, 40)
	binary.LittleEndian.PutUint32(hdr[0:], 40)                // biSize
	binary.LittleEndian.PutUint32(hdr[4:], uint32(size))      // biWidth
	binary.LittleEndian.PutUint32(hdr[8:], uint32(size*2))    // biHeight (XOR+AND)
	binary.LittleEndian.PutUint16(hdr[12:], 1)                // biPlanes
	binary.LittleEndian.PutUint16(hdr[14:], 32)               // biBitCount
	binary.LittleEndian.PutUint32(hdr[20:], uint32(len(pix))) // biSizeImage

	buf.Write(hdr)
	buf.Write(pix)
	buf.Write(mask)
	return buf.Bytes()
}

// RenderPNG renders the filled severity disc as PNG bytes (toast logo).
func RenderPNG(c RGB, size int) []byte { return pngFrom(circlePixel, c, size) }

// pngFrom renders any pixel function into a PNG buffer.
func pngFrom(pixel pixelFn, c RGB, size int) []byte {
	img := image.NewNRGBA(image.Rect(0, 0, size, size))
	for y := range size {
		for x := range size {
			r, g, b, a := pixel(c, size, x, y)
			img.SetNRGBA(x, y, color.NRGBA{R: r, G: g, B: b, A: a})
		}
	}
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		return nil
	}
	return buf.Bytes()
}

// circlePixel composes a colored disc over a slightly larger white halo,
// both antialiased, over transparency — visible on light and dark trays.
func circlePixel(c RGB, size, x, y int) (uint8, uint8, uint8, uint8) {
	d := dist(size, x, y)
	discR := center(size) - 3.0
	discA := coverage(d, discR)
	haloA := coverage(d, discR+1.5)
	if haloA <= 0 {
		return 0, 0, 0, 0
	}
	return blendOverWhite(c, discA/haloA, haloA)
}

// ringPixel composes a severity-colored ring (3px band) over the same
// white halo — reads as an "off" phase next to the filled disc.
func ringPixel(c RGB, size, x, y int) (uint8, uint8, uint8, uint8) {
	d := dist(size, x, y)
	r := center(size) - 3.0
	ringA := coverage(d, r) - coverage(d, r-3)
	haloA := coverage(d, r+1.5) - coverage(d, r) // halo hugs the ring outside
	if haloA <= 0 && ringA <= 0 {
		return 0, 0, 0, 0
	}
	alpha := math.Max(haloA, ringA)
	return blendOverWhite(c, ringA/alpha, alpha)
}

// blendOverWhite composes color at mix over a white base with alpha.
func blendOverWhite(c RGB, mix, alpha float64) (uint8, uint8, uint8, uint8) {
	out := func(ch uint8) uint8 {
		v := 255*(1-mix) + float64(ch)*mix
		if v < 0 {
			v = 0
		} else if v > 255 {
			v = 255
		}
		return uint8(v + 0.5)
	}
	return out(c.R), out(c.G), out(c.B), uint8(alpha*255 + 0.5)
}

func center(size int) float64 { return float64(size) / 2 }

func dist(size, x, y int) float64 {
	dx := float64(x) + 0.5 - center(size)
	dy := float64(y) + 0.5 - center(size)
	return math.Hypot(dx, dy)
}

// coverage approximates the fraction of the pixel covered by a disc of
// radius r at distance d (1px linear ramp).
func coverage(d, r float64) float64 {
	return clamp01(r - d + 0.5)
}

func clamp01(v float64) float64 {
	if v < 0 {
		return 0
	}
	if v > 1 {
		return 1
	}
	return v
}
