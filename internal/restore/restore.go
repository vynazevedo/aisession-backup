package restore

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

func Restore(backupPath, claudeDir string, pathMappings map[string]string, dryRun bool) error {
	f, err := os.Open(backupPath)
	if err != nil {
		return fmt.Errorf("failed to open backup: %w", err)
	}
	defer f.Close()

	gzReader, err := gzip.NewReader(f)
	if err != nil {
		return fmt.Errorf("failed to read gzip: %w", err)
	}
	defer gzReader.Close()

	tarReader := tar.NewReader(gzReader)

	var manifest Manifest
	sessionData := make(map[string]map[string][]byte)

	for {
		header, err := tarReader.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return fmt.Errorf("failed to read tar: %w", err)
		}

		if !utils.IsPathSafe(".", header.Name) {
			utils.LogVerbose("Skipping unsafe path: %s", header.Name)
			continue
		}

		data, err := io.ReadAll(tarReader)
		if err != nil {
			utils.LogVerbose("Failed to read %s: %v", header.Name, err)
			continue
		}

		if header.Name == "manifest.json" {
			if err := json.Unmarshal(data, &manifest); err != nil {
				return fmt.Errorf("failed to parse manifest: %w", err)
			}
			continue
		}

		parts := strings.Split(header.Name, "/")
		if len(parts) >= 3 && parts[0] == "sessions" {
			sessionID := parts[1]
			fileName := parts[2]

			if sessionData[sessionID] == nil {
				sessionData[sessionID] = make(map[string][]byte)
			}
			sessionData[sessionID][fileName] = data
		}
	}

	if len(manifest.Sessions) == 0 {
		return fmt.Errorf("invalid backup: no sessions found")
	}

	if dryRun {
		fmt.Println("=== DRY RUN MODE ===")
		fmt.Printf("Would restore %d sessions:\n\n", len(manifest.Sessions))
	} else {
		fmt.Printf("Restoring %d sessions...\n\n", len(manifest.Sessions))
		if err := os.MkdirAll(filepath.Join(claudeDir, "projects"), 0755); err != nil {
			return fmt.Errorf("failed to create projects dir: %w", err)
		}
		if err := os.MkdirAll(filepath.Join(claudeDir, "session-env"), 0755); err != nil {
			return fmt.Errorf("failed to create session-env dir: %w", err)
		}
		if err := os.MkdirAll(filepath.Join(claudeDir, "todos"), 0755); err != nil {
			return fmt.Errorf("failed to create todos dir: %w", err)
		}
	}

	var restored, skipped, failed int

	for _, sess := range manifest.Sessions {
		data, exists := sessionData[sess.ID]
		if !exists {
			utils.LogVerbose("Session data not found for %s", sess.ID)
			continue
		}

		newDir := sess.Directory
		for oldPath, newPath := range pathMappings {
			if strings.HasPrefix(sess.Directory, oldPath) {
				newDir = newPath + sess.Directory[len(oldPath):]
				break
			}
		}

		encodedPath := strings.ReplaceAll(newDir, "/", "-")
		projectDir := filepath.Join(claudeDir, "projects", encodedPath)
		sessionPath := filepath.Join(projectDir, sess.ID+".jsonl")

		if _, err := os.Stat(sessionPath); err == nil && !dryRun {
			fmt.Printf("Warning: %s\n", sess.Directory)
			fmt.Printf("   Session already exists, skipping\n")
			skipped++
			continue
		}

		fmt.Printf("  %s\n", sess.Directory)
		if newDir != sess.Directory {
			fmt.Printf("   -> %s\n", newDir)
		}

		if dryRun {
			fmt.Printf("   [dry-run] Would restore session\n")
			restored++
			continue
		}

		if !utils.IsPathSafe(claudeDir, projectDir) {
			fmt.Printf("   ERROR: Unsafe path detected, skipping\n")
			failed++
			continue
		}

		if err := os.MkdirAll(projectDir, 0755); err != nil {
			fmt.Printf("   ERROR: Failed to create directory: %v\n", err)
			failed++
			continue
		}

		if sessionContent, ok := data[sess.ID+".jsonl"]; ok {
			if err := os.WriteFile(sessionPath, sessionContent, 0644); err != nil {
				fmt.Printf("   ERROR: Failed to write session: %v\n", err)
				failed++
				continue
			}
			fmt.Printf("   OK: Session restored\n")
			restored++
		}

		if envContent, ok := data["env.json"]; ok {
			envPath := filepath.Join(claudeDir, "session-env", sess.ID+".json")
			envStr := string(envContent)
			for oldPath, newPath := range pathMappings {
				envStr = strings.ReplaceAll(envStr, oldPath, newPath)
			}
			if err := os.WriteFile(envPath, []byte(envStr), 0644); err != nil {
				utils.LogVerbose("Failed to write env file: %v", err)
			}
		}

		if todoContent, ok := data["todos.json"]; ok {
			todoPath := filepath.Join(claudeDir, "todos", sess.ID+".json")
			if err := os.WriteFile(todoPath, todoContent, 0644); err != nil {
				utils.LogVerbose("Failed to write todos file: %v", err)
			}
		}

		historyEntry := map[string]interface{}{
			"sessionId": sess.ID,
			"directory": newDir,
			"timestamp": sess.Timestamp.Format(time.RFC3339),
			"model":     sess.Model,
		}

		historyPath := filepath.Join(claudeDir, "history.jsonl")
		historyFile, err := os.OpenFile(historyPath, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0644)
		if err == nil {
			entryData, err := json.Marshal(historyEntry)
			if err == nil {
				historyFile.Write(entryData)
				historyFile.WriteString("\n")
			}
			historyFile.Close()
		}
	}

	fmt.Println()
	if dryRun {
		fmt.Printf("=== DRY RUN SUMMARY ===\n")
		fmt.Printf("Would restore: %d sessions\n", restored)
	} else {
		fmt.Printf("Restore complete!\n")
		fmt.Printf("   Restored: %d | Skipped: %d | Failed: %d\n", restored, skipped, failed)
	}
	return nil
}

func RunInteractive(backupPath, claudeDir string, dryRun bool) error {
	f, err := os.Open(backupPath)
	if err != nil {
		return err
	}

	gzReader, err := gzip.NewReader(f)
	if err != nil {
		f.Close()
		return err
	}

	tarReader := tar.NewReader(gzReader)
	var manifest Manifest

	for {
		header, err := tarReader.Next()
		if err != nil {
			break
		}
		if header.Name == "manifest.json" {
			data, _ := io.ReadAll(tarReader)
			json.Unmarshal(data, &manifest)
			break
		}
	}
	gzReader.Close()
	f.Close()

	if len(manifest.Sessions) == 0 {
		return fmt.Errorf("invalid backup file")
	}

	fmt.Println("====================================================================")
	fmt.Println("              Claude Code Session Restore                           ")
	fmt.Println("====================================================================")
	fmt.Println()
	fmt.Printf("Backup created: %s\n", manifest.CreatedAt.Format("2006-01-02 15:04:05"))
	fmt.Printf("Source: %s (%s)\n", manifest.SourceHost, manifest.SourceOS)
	fmt.Printf("Sessions: %d\n\n", len(manifest.Sessions))

	basePaths := make(map[string]bool)
	for _, sess := range manifest.Sessions {
		parts := strings.Split(sess.Directory, "/")
		if len(parts) >= 3 {
			basePath := "/" + parts[1] + "/" + parts[2]
			basePaths[basePath] = true
		}
	}

	fmt.Println("Sessions in backup:")
	for _, sess := range manifest.Sessions {
		fmt.Printf("  - %s (%d messages)\n", sess.Directory, sess.MessageCount)
	}
	fmt.Println()

	pathMappings := make(map[string]string)

	fmt.Println("Path Mapping")
	fmt.Println("------------")
	fmt.Println("Enter the new path for each source path (or press Enter to keep same):")
	fmt.Println()

	reader := os.Stdin
	scanner := make([]byte, 1024)

	for basePath := range basePaths {
		fmt.Printf("  %s\n", basePath)
		fmt.Print("  -> ")

		n, _ := reader.Read(scanner)
		newPath := strings.TrimSpace(string(scanner[:n]))

		if newPath != "" && newPath != basePath {
			pathMappings[basePath] = newPath
			fmt.Printf("     Mapped to: %s\n", newPath)
		}
		fmt.Println()
	}

	fmt.Print("Proceed with restore? [y/N]: ")
	n, _ := reader.Read(scanner)
	confirm := strings.ToLower(strings.TrimSpace(string(scanner[:n])))

	if confirm != "y" && confirm != "yes" {
		fmt.Println("Cancelled.")
		return nil
	}

	fmt.Println()
	return Restore(backupPath, claudeDir, pathMappings, dryRun)
}
