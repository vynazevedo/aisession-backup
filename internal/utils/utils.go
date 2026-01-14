package utils

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

var Verbose = false

func LogVerbose(format string, args ...interface{}) {
	if Verbose {
		fmt.Printf("[DEBUG] "+format+"\n", args...)
	}
}

func IsPathSafe(basePath, targetPath string) bool {
	absBase, err := filepath.Abs(basePath)
	if err != nil {
		return false
	}
	absTarget, err := filepath.Abs(targetPath)
	if err != nil {
		return false
	}
	rel, err := filepath.Rel(absBase, absTarget)
	if err != nil {
		return false
	}
	return !strings.HasPrefix(rel, "..")
}

func ValidatePathMapping(mapping string) error {
	parts := strings.SplitN(mapping, ":", 2)
	if len(parts) != 2 {
		return fmt.Errorf("invalid mapping format: %s (expected old:new)", mapping)
	}
	if !filepath.IsAbs(parts[0]) || !filepath.IsAbs(parts[1]) {
		return fmt.Errorf("paths must be absolute: %s", mapping)
	}
	return nil
}

func CalculateChecksum(data []byte) string {
	hash := sha256.Sum256(data)
	return hex.EncodeToString(hash[:])
}

func HumanSize(bytes int64) string {
	const unit = 1024
	if bytes < unit {
		return fmt.Sprintf("%d B", bytes)
	}
	div, exp := int64(unit), 0
	for n := bytes / unit; n >= unit; n /= unit {
		div *= unit
		exp++
	}
	return fmt.Sprintf("%.1f %cB", float64(bytes)/float64(div), "KMGTPE"[exp])
}

func ExpandPath(path string) string {
	if strings.HasPrefix(path, "~/") {
		home, _ := os.UserHomeDir()
		return filepath.Join(home, path[2:])
	}
	return path
}

func GetOS() string {
	if _, err := os.Stat("/etc/os-release"); err == nil {
		return "linux"
	}
	if _, err := os.Stat("/System/Library"); err == nil {
		return "darwin"
	}
	return "unknown"
}
