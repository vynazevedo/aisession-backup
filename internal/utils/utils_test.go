package utils

import (
	"os"
	"path/filepath"
	"testing"
)

func TestIsPathSafe(t *testing.T) {
	tests := []struct {
		name     string
		basePath string
		target   string
		want     bool
	}{
		{
			name:     "safe path within base",
			basePath: "/home/user/.claude",
			target:   "/home/user/.claude/projects/test",
			want:     true,
		},
		{
			name:     "path traversal attack",
			basePath: "/home/user/.claude",
			target:   "/home/user/.claude/../../../etc/passwd",
			want:     false,
		},
		{
			name:     "path traversal with dots",
			basePath: "/home/user/.claude",
			target:   "/home/user/.claude/projects/../../..",
			want:     false,
		},
		{
			name:     "same directory",
			basePath: "/home/user/.claude",
			target:   "/home/user/.claude",
			want:     true,
		},
		{
			name:     "relative safe path",
			basePath: ".",
			target:   "sessions/abc123",
			want:     true,
		},
		{
			name:     "relative unsafe path",
			basePath: ".",
			target:   "../../../etc/passwd",
			want:     false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := IsPathSafe(tt.basePath, tt.target)
			if got != tt.want {
				t.Errorf("IsPathSafe(%q, %q) = %v, want %v", tt.basePath, tt.target, got, tt.want)
			}
		})
	}
}

func TestValidatePathMapping(t *testing.T) {
	tests := []struct {
		name    string
		mapping string
		wantErr bool
	}{
		{
			name:    "valid mapping",
			mapping: "/home/user:/Users/user",
			wantErr: false,
		},
		{
			name:    "missing colon",
			mapping: "/home/user/Users/user",
			wantErr: true,
		},
		{
			name:    "relative source path",
			mapping: "home/user:/Users/user",
			wantErr: true,
		},
		{
			name:    "relative target path",
			mapping: "/home/user:Users/user",
			wantErr: true,
		},
		{
			name:    "empty mapping",
			mapping: "",
			wantErr: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			err := ValidatePathMapping(tt.mapping)
			if (err != nil) != tt.wantErr {
				t.Errorf("ValidatePathMapping(%q) error = %v, wantErr %v", tt.mapping, err, tt.wantErr)
			}
		})
	}
}

func TestCalculateChecksum(t *testing.T) {
	data := []byte("test data for checksum")
	checksum := CalculateChecksum(data)

	if len(checksum) != 64 {
		t.Errorf("CalculateChecksum() returned checksum with length %d, want 64", len(checksum))
	}

	checksum2 := CalculateChecksum(data)
	if checksum != checksum2 {
		t.Error("CalculateChecksum() should return same result for same input")
	}

	differentData := []byte("different data")
	checksum3 := CalculateChecksum(differentData)
	if checksum == checksum3 {
		t.Error("CalculateChecksum() should return different result for different input")
	}
}

func TestHumanSize(t *testing.T) {
	tests := []struct {
		bytes int64
		want  string
	}{
		{0, "0 B"},
		{100, "100 B"},
		{1024, "1.0 KB"},
		{1536, "1.5 KB"},
		{1048576, "1.0 MB"},
		{1073741824, "1.0 GB"},
	}

	for _, tt := range tests {
		t.Run(tt.want, func(t *testing.T) {
			got := HumanSize(tt.bytes)
			if got != tt.want {
				t.Errorf("HumanSize(%d) = %q, want %q", tt.bytes, got, tt.want)
			}
		})
	}
}

func TestExpandPath(t *testing.T) {
	home, _ := os.UserHomeDir()

	tests := []struct {
		path string
		want string
	}{
		{"~/.claude", filepath.Join(home, ".claude")},
		{"/absolute/path", "/absolute/path"},
		{"relative/path", "relative/path"},
	}

	for _, tt := range tests {
		t.Run(tt.path, func(t *testing.T) {
			got := ExpandPath(tt.path)
			if got != tt.want {
				t.Errorf("ExpandPath(%q) = %q, want %q", tt.path, got, tt.want)
			}
		})
	}
}

func TestGetOS(t *testing.T) {
	os := GetOS()
	validOS := map[string]bool{"linux": true, "darwin": true, "unknown": true}

	if !validOS[os] {
		t.Errorf("GetOS() = %q, want one of linux, darwin, unknown", os)
	}
}
