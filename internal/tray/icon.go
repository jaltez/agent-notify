package tray

import (
	"bytes"
	"encoding/binary"
	"image"
	"image/color"
	"image/png"
	"math"
	"runtime"
)

// rgb is an 8-bit color channel triple.
type rgb struct{ R, G, B uint8 }

// severityColors maps the engine severity to tray icon colors:
// blocked red, offline amber, waiting-for-you green, working blue,
// idle gray.
var severityColors = map[string]rgb{
	"blocked": {0xE5, 0x48, 0x4D},
	"down":    {0xFF, 0xB0, 0x2E},
	"waiting": {0x22, 0xC5, 0x5E},
	"working": {0x3B, 0x82, 0xF6},
	"idle":    {0x6B, 0x72, 0x80},
}

// IconBytes returns tray icon bytes for a severity: a 32bpp ICO on
// Windows (LoadImage-compatible), a PNG elsewhere.
func IconBytes(severity string, size int) []byte {
	c, ok := severityColors[severity]
	if !ok {
		c = severityColors["idle"]
	}
	if runtime.GOOS == "windows" {
		return ICO(c, size)
	}
	return PNG(c, size)
}

// ICO renders a filled, antialiased disc with a thin white halo as a
// 32bpp DIB inside an ICO container — no PNG-in-ICO support needed.
func ICO(c rgb, size int) []byte {
	dib := dibCircle(c, size)
	// Fully transparent AND mask (alpha channel carries opacity), rows
	// padded to 4-byte boundaries as the ICO format requires.
	maskRowBytes := ((size+7)/8 + 3) / 4 * 4
	mask := make([]byte, maskRowBytes*size)

	var buf bytes.Buffer
	binary.Write(&buf, binary.LittleEndian, uint16(0)) // reserved
	binary.Write(&buf, binary.LittleEndian, uint16(1)) // type: icon
	binary.Write(&buf, binary.LittleEndian, uint16(1)) // image count
	dim := byte(size)
	if size >= 256 {
		dim = 0 // ICO convention for 256px
	}
	buf.WriteByte(dim)                                                  // width
	buf.WriteByte(dim)                                                  // height
	buf.WriteByte(0)                                                    // palette colors
	buf.WriteByte(0)                                                    // reserved
	binary.Write(&buf, binary.LittleEndian, uint16(1))                  // color planes
	binary.Write(&buf, binary.LittleEndian, uint16(32))                 // bits per pixel
	binary.Write(&buf, binary.LittleEndian, uint32(len(dib)+len(mask))) // data size
	binary.Write(&buf, binary.LittleEndian, uint32(22))                 // data offset (6 + 16)
	buf.Write(dib)
	buf.Write(mask)
	return buf.Bytes()
}

// dibCircle renders the icon as a BITMAPINFOHEADER plus bottom-up BGRA
// rows. Height is doubled per the ICO spec (XOR + AND masks).
func dibCircle(c rgb, size int) []byte {
	pixels := make([]byte, 0, size*size*4)
	for y := size - 1; y >= 0; y-- { // bottom-up
		for x := 0; x < size; x++ {
			r, g, b, a := circlePixel(c, size, x, y)
			pixels = append(pixels, b, g, r, a)
		}
	}
	hdr := make([]byte, 40)
	binary.LittleEndian.PutUint32(hdr[0:], 40)                   // biSize
	binary.LittleEndian.PutUint32(hdr[4:], uint32(size))         // biWidth
	binary.LittleEndian.PutUint32(hdr[8:], uint32(size*2))       // biHeight (XOR+AND)
	binary.LittleEndian.PutUint16(hdr[12:], 1)                   // biPlanes
	binary.LittleEndian.PutUint16(hdr[14:], 32)                  // biBitCount
	binary.LittleEndian.PutUint32(hdr[20:], uint32(len(pixels))) // biSizeImage

	var buf bytes.Buffer
	buf.Write(hdr)
	buf.Write(pixels)
	return buf.Bytes()
}

// circlePixel composes a colored disc over a slightly larger white halo,
// both antialiased, over transparency — visible on light and dark trays.
func circlePixel(c rgb, size, x, y int) (uint8, uint8, uint8, uint8) {
	fx := float64(x) + 0.5
	fy := float64(y) + 0.5
	center := float64(size) / 2
	dx, dy := fx-center, fy-center
	d := math.Hypot(dx, dy)
	discR := center - 3.0
	haloR := discR + 1.5

	discA := coverage(d, discR)
	haloA := coverage(d, haloR)
	if haloA <= 0 {
		return 0, 0, 0, 0
	}
	mix := 0.0
	if haloA > 0 {
		mix = discA / haloA // 0 = white halo, 1 = full color
	}
	out := func(ch uint8) uint8 {
		v := 255*(1-mix) + float64(ch)*mix
		if v < 0 {
			v = 0
		} else if v > 255 {
			v = 255
		}
		return uint8(v + 0.5)
	}
	return out(c.R), out(c.G), out(c.B), uint8(haloA*255 + 0.5)
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

// PNG renders the same disc for non-Windows trays.
func PNG(c rgb, size int) []byte {
	img := image.NewNRGBA(image.Rect(0, 0, size, size))
	for y := range size {
		for x := range size {
			r, g, b, a := circlePixel(c, size, x, y)
			img.SetNRGBA(x, y, color.NRGBA{R: r, G: g, B: b, A: a})
		}
	}
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		return nil
	}
	return buf.Bytes()
}
