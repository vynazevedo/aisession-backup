package session

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestCountMessages(t *testing.T) {
	tmpDir := t.TempDir()
	sessionFile := filepath.Join(tmpDir, "test.jsonl")

	messages := []map[string]interface{}{
		{"type": "user", "message": map[string]interface{}{"content": "Hello, this is a test message"}},
		{"type": "assistant", "message": map[string]interface{}{"content": "Hi there!"}},
		{"type": "user", "message": map[string]interface{}{"content": "Another message"}},
	}

	f, err := os.Create(sessionFile)
	if err != nil {
		t.Fatal(err)
	}
	encoder := json.NewEncoder(f)
	for _, msg := range messages {
		encoder.Encode(msg)
	}
	f.Close()

	count, firstPreview, lastPreview := CountMessages(sessionFile)

	if count != 3 {
		t.Errorf("CountMessages() count = %d, want 3", count)
	}

	if firstPreview != "Hello, this is a test message" {
		t.Errorf("CountMessages() firstPreview = %q, want %q", firstPreview, "Hello, this is a test message")
	}

	if lastPreview != "Another message" {
		t.Errorf("CountMessages() lastPreview = %q, want %q", lastPreview, "Another message")
	}
}

func TestCountMessagesEmptyFile(t *testing.T) {
	tmpDir := t.TempDir()
	sessionFile := filepath.Join(tmpDir, "empty.jsonl")

	f, _ := os.Create(sessionFile)
	f.Close()

	count, firstPreview, lastPreview := CountMessages(sessionFile)

	if count != 0 {
		t.Errorf("CountMessages() count = %d, want 0", count)
	}

	if firstPreview != "" {
		t.Errorf("CountMessages() firstPreview = %q, want empty string", firstPreview)
	}

	if lastPreview != "" {
		t.Errorf("CountMessages() lastPreview = %q, want empty string", lastPreview)
	}
}

func TestCountMessagesNonExistent(t *testing.T) {
	count, firstPreview, lastPreview := CountMessages("/nonexistent/file.jsonl")

	if count != 0 {
		t.Errorf("CountMessages() count = %d, want 0", count)
	}

	if firstPreview != "" {
		t.Errorf("CountMessages() firstPreview = %q, want empty string", firstPreview)
	}

	if lastPreview != "" {
		t.Errorf("CountMessages() lastPreview = %q, want empty string", lastPreview)
	}
}

func TestCountMessagesLongPreview(t *testing.T) {
	tmpDir := t.TempDir()
	sessionFile := filepath.Join(tmpDir, "long.jsonl")

	longMessage := "This is a very long message that should be truncated because it exceeds sixty characters in length"
	messages := []map[string]interface{}{
		{"type": "user", "message": map[string]interface{}{"content": longMessage}},
	}

	f, _ := os.Create(sessionFile)
	encoder := json.NewEncoder(f)
	for _, msg := range messages {
		encoder.Encode(msg)
	}
	f.Close()

	_, firstPreview, lastPreview := CountMessages(sessionFile)

	if len(firstPreview) > 60 {
		t.Errorf("CountMessages() firstPreview length = %d, want <= 60", len(firstPreview))
	}

	if firstPreview[len(firstPreview)-3:] != "..." {
		t.Error("CountMessages() firstPreview should end with ...")
	}

	if firstPreview != lastPreview {
		t.Error("CountMessages() firstPreview should equal lastPreview for single message")
	}
}

func TestLoadSessions(t *testing.T) {
	tmpDir := t.TempDir()

	projectsDir := filepath.Join(tmpDir, "projects", "-home-user-project")
	os.MkdirAll(projectsDir, 0755)

	sessionID := "abc123-def456"
	sessionFile := filepath.Join(projectsDir, sessionID+".jsonl")

	messages := []map[string]interface{}{
		{"type": "user", "message": map[string]interface{}{"content": "Test"}},
	}
	f, _ := os.Create(sessionFile)
	encoder := json.NewEncoder(f)
	for _, msg := range messages {
		encoder.Encode(msg)
	}
	f.Close()

	sessions, err := LoadSessions(tmpDir)
	if err != nil {
		t.Fatal(err)
	}

	if len(sessions) != 1 {
		t.Fatalf("LoadSessions() returned %d sessions, want 1", len(sessions))
	}

	if sessions[0].ID != sessionID {
		t.Errorf("LoadSessions() session ID = %q, want %q", sessions[0].ID, sessionID)
	}

	if sessions[0].Directory != "/home/user/project" {
		t.Errorf("LoadSessions() session Directory = %q, want %q", sessions[0].Directory, "/home/user/project")
	}

	if sessions[0].MessageCount != 1 {
		t.Errorf("LoadSessions() session MessageCount = %d, want 1", sessions[0].MessageCount)
	}
}

func TestLoadSessionsEmptyDir(t *testing.T) {
	tmpDir := t.TempDir()

	projectsDir := filepath.Join(tmpDir, "projects")
	os.MkdirAll(projectsDir, 0755)

	sessions, err := LoadSessions(tmpDir)
	if err != nil {
		t.Fatal(err)
	}

	if len(sessions) != 0 {
		t.Errorf("LoadSessions() returned %d sessions, want 0", len(sessions))
	}
}

func TestLoadSessionsNoProjectsDir(t *testing.T) {
	tmpDir := t.TempDir()

	_, err := LoadSessions(tmpDir)
	if err == nil {
		t.Error("LoadSessions() should return error when projects dir doesn't exist")
	}
}
