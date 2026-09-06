package tray

import (
	"encoding/binary"
	"testing"
)

func TestICOStructure(t *testing.T) {
	const size = 32
	data := ICO(severityColors["blocked"], size)
	if len(data) == 0 {
		t.Fatal("empty ICO")
	}
	// ICONDIR: reserved 0, type icon, one image.
	if binary.LittleEndian.Uint16(data[0:]) != 0 ||
		binary.LittleEndian.Uint16(data[2:]) != 1 ||
		binary.LittleEndian.Uint16(data[4:]) != 1 {
		t.Fatalf("bad ICONDIR: % x", data[:6])
	}
	// ICONDIRENTRY (per spec: planes@10, bpp@12, bytesInRes@14, offset@18)
	if data[6] != size || data[7] != size {
		t.Errorf("dimensions: %dx%d", data[6], data[7])
	}
	if got := binary.LittleEndian.Uint16(data[10:]); got != 1 {
		t.Errorf("planes = %d", got)
	}
	if got := binary.LittleEndian.Uint16(data[12:]); got != 32 {
		t.Errorf("bpp = %d", got)
	}
	payload := binary.LittleEndian.Uint32(data[14:])
	offset := binary.LittleEndian.Uint32(data[18:])
	if offset != 22 {
		t.Errorf("offset = %d", offset)
	}
	if int(offset+payload) != len(data) {
		t.Errorf("payload %d + offset %d != total %d", payload, offset, len(data))
	}

	// DIB header: 40 bytes, width, double height (XOR + AND), 32bpp.
	dib := data[offset:]
	if got := binary.LittleEndian.Uint32(dib[0:]); got != 40 {
		t.Fatalf("biSize = %d", got)
	}
	if got := binary.LittleEndian.Uint32(dib[4:]); got != size {
		t.Errorf("biWidth = %d", got)
	}
	if got := binary.LittleEndian.Uint32(dib[8:]); got != size*2 {
		t.Errorf("biHeight = %d", got)
	}
	if got := binary.LittleEndian.Uint16(dib[14:]); got != 32 {
		t.Errorf("biBitCount = %d", got)
	}

	// Pixel checks over the BGRA rows (stored bottom-up).
	pix := dib[40:]
	at := func(x, y int) []byte { // x,y in visual (top-down) coordinates
		row := size - 1 - y
		return pix[(row*size+x)*4 : (row*size+x)*4+4]
	}
	center := at(size/2, size/2)
	if center[3] == 0 {
		t.Error("center pixel transparent")
	}
	if center[0] == 0 && center[1] == 0 && center[2] == 0 {
		t.Errorf("center pixel colorless: % x", center)
	}
	for _, corner := range [][2]int{{0, 0}, {size - 1, 0}, {0, size - 1}, {size - 1, size - 1}} {
		if p := at(corner[0], corner[1]); p[3] != 0 {
			t.Errorf("corner %v opaque: % x", corner, p)
		}
	}
}

func TestSeverityColorsComplete(t *testing.T) {
	for _, sev := range []string{"blocked", "down", "waiting", "working", "idle"} {
		if len(IconBytes(sev, 32)) == 0 {
			t.Errorf("no icon bytes for %q", sev)
		}
	}
	// unknown severity falls back to idle, not empty
	if len(IconBytes("nonsense", 32)) == 0 {
		t.Error("unknown severity produced no icon")
	}
}
