package backup

import (
	"archive/tar"
	"compress/gzip"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	"claude-migrate/internal/session"
	"claude-migrate/internal/utils"
)

type Manifest struct {
	Version    string            `json:"version"`
	CreatedAt  time.Time         `json:"created_at"`
	SourceHost string            `json:"source_host,omitempty"`
	SourceOS   string            `json:"source_os,omitempty"`
	Sessions   []session.Session `json:"sessions"`
	Checksum   string            `json:"checksum,omitempty"`
	TotalSize  int64             `json:"total_size,omitempty"`
}

func Create(claudeDir string, sessions []session.Session, outputPath string) error {
	outFile, err := os.Create(outputPath)
	if err != nil {
		return fmt.Errorf("failed to create output file: %w", err)
	}
	defer outFile.Close()

	gzWriter := gzip.NewWriter(outFile)
	defer gzWriter.Close()

	tarWriter := tar.NewWriter(gzWriter)
	defer tarWriter.Close()

	var totalSize int64
	var allData []byte
	var errors []string

	for _, sess := range sessions {
		encodedPath := strings.ReplaceAll(sess.Directory, "/", "-")
		sessionFile := filepath.Join(claudeDir, "projects", encodedPath, sess.ID+".jsonl")

		if data, err := os.ReadFile(sessionFile); err == nil {
			totalSize += int64(len(data))
			allData = append(allData, data...)
		}
	}

	hostname, _ := os.Hostname()
	manifest := Manifest{
		Version:    "1.0",
		CreatedAt:  time.Now(),
		SourceHost: hostname,
		SourceOS:   utils.GetOS(),
		Sessions:   sessions,
		Checksum:   utils.CalculateChecksum(allData),
		TotalSize:  totalSize,
	}

	manifestData, err := json.MarshalIndent(manifest, "", "  ")
	if err != nil {
		return fmt.Errorf("failed to create manifest: %w", err)
	}

	if err := addToTar(tarWriter, "manifest.json", manifestData); err != nil {
		return fmt.Errorf("failed to write manifest: %w", err)
	}

	for _, sess := range sessions {
		encodedPath := strings.ReplaceAll(sess.Directory, "/", "-")
		sessionFile := filepath.Join(claudeDir, "projects", encodedPath, sess.ID+".jsonl")

		if data, err := os.ReadFile(sessionFile); err == nil {
			tarPath := fmt.Sprintf("sessions/%s/%s.jsonl", sess.ID, sess.ID)
			if err := addToTar(tarWriter, tarPath, data); err != nil {
				errors = append(errors, fmt.Sprintf("session %s: %v", sess.ID, err))
			}
			utils.LogVerbose("Added session: %s (%s)", sess.ID, utils.HumanSize(int64(len(data))))
		} else {
			utils.LogVerbose("Session file not found: %s", sessionFile)
		}

		envFile := filepath.Join(claudeDir, "session-env", sess.ID+".json")
		if data, err := os.ReadFile(envFile); err == nil {
			tarPath := fmt.Sprintf("sessions/%s/env.json", sess.ID)
			if err := addToTar(tarWriter, tarPath, data); err != nil {
				utils.LogVerbose("Failed to add env for %s: %v", sess.ID, err)
			}
		}

		todoFile := filepath.Join(claudeDir, "todos", sess.ID+".json")
		if data, err := os.ReadFile(todoFile); err == nil {
			tarPath := fmt.Sprintf("sessions/%s/todos.json", sess.ID)
			if err := addToTar(tarWriter, tarPath, data); err != nil {
				utils.LogVerbose("Failed to add todos for %s: %v", sess.ID, err)
			}
		}
	}

	if len(errors) > 0 {
		fmt.Printf("Warning: Backup completed with %d error(s):\n", len(errors))
		for _, e := range errors {
			fmt.Printf("   - %s\n", e)
		}
	}

	return nil
}

func List(backupPath string) error {
	f, err := os.Open(backupPath)
	if err != nil {
		return err
	}
	defer f.Close()

	gzReader, err := gzip.NewReader(f)
	if err != nil {
		return err
	}
	defer gzReader.Close()

	tarReader := tar.NewReader(gzReader)
	var manifest Manifest

	for {
		header, err := tarReader.Next()
		if err != nil {
			break
		}
		if header.Name == "manifest.json" {
			data, err := io.ReadAll(tarReader)
			if err != nil {
				return fmt.Errorf("failed to read manifest: %w", err)
			}
			if err := json.Unmarshal(data, &manifest); err != nil {
				return fmt.Errorf("failed to parse manifest: %w", err)
			}
			break
		}
	}

	if len(manifest.Sessions) == 0 {
		return fmt.Errorf("invalid backup file")
	}

	fmt.Println("====================================================================")
	fmt.Printf("  Backup: %-51s\n", filepath.Base(backupPath))
	fmt.Println("====================================================================")
	fmt.Println()
	fmt.Printf("Created: %s\n", manifest.CreatedAt.Format("2006-01-02 15:04:05"))
	fmt.Printf("Source:  %s (%s)\n", manifest.SourceHost, manifest.SourceOS)
	fmt.Printf("Version: %s\n", manifest.Version)
	if manifest.Checksum != "" {
		fmt.Printf("Checksum: %s\n", manifest.Checksum[:16]+"...")
	}
	if manifest.TotalSize > 0 {
		fmt.Printf("Total Size: %s\n", utils.HumanSize(manifest.TotalSize))
	}
	fmt.Println()
	fmt.Printf("Sessions (%d):\n\n", len(manifest.Sessions))

	for _, sess := range manifest.Sessions {
		fmt.Printf("  %s\n", sess.Directory)
		fmt.Printf("     ID: %s\n", sess.ID)
		fmt.Printf("     Messages: %d | Size: %s | Date: %s\n",
			sess.MessageCount,
			utils.HumanSize(sess.SizeBytes),
			sess.Timestamp.Format("2006-01-02 15:04"))
		fmt.Println()
	}

	return nil
}

func addToTar(tw *tar.Writer, name string, data []byte) error {
	header := &tar.Header{
		Name:    name,
		Mode:    0644,
		Size:    int64(len(data)),
		ModTime: time.Now(),
	}
	if err := tw.WriteHeader(header); err != nil {
		return err
	}
	_, err := tw.Write(data)
	return err
}
