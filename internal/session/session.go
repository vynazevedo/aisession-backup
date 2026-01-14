package session

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

type Session struct {
	ID           string    `json:"id"`
	Directory    string    `json:"directory"`
	Timestamp    time.Time `json:"timestamp"`
	Model        string    `json:"model,omitempty"`
	MessageCount int       `json:"message_count"`
	SizeBytes    int64     `json:"size_bytes"`
	Preview      string    `json:"preview,omitempty"`
	LastPreview  string    `json:"last_preview,omitempty"`
}

func LoadSessions(claudeDir string) ([]Session, error) {
	var sessions []Session
	sessionsMap := make(map[string]*Session)

	historyPath := filepath.Join(claudeDir, "history.jsonl")
	if f, err := os.Open(historyPath); err == nil {
		defer f.Close()
		decoder := json.NewDecoder(f)
		for {
			var entry map[string]interface{}
			if err := decoder.Decode(&entry); err != nil {
				break
			}

			sid, _ := entry["sessionId"].(string)
			dir, _ := entry["directory"].(string)
			timestamp, _ := entry["timestamp"].(string)
			model, _ := entry["model"].(string)

			if sid == "" || dir == "" {
				continue
			}

			sess := &Session{
				ID:        sid,
				Directory: dir,
				Model:     model,
			}

			if t, err := time.Parse(time.RFC3339, timestamp); err == nil {
				sess.Timestamp = t
			}

			sessionsMap[sid] = sess
		}
	}

	projectsDir := filepath.Join(claudeDir, "projects")
	entries, err := os.ReadDir(projectsDir)
	if err != nil {
		return nil, err
	}

	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}

		encodedPath := entry.Name()
		decodedPath := "/" + strings.ReplaceAll(encodedPath[1:], "-", "/")

		sessionDir := filepath.Join(projectsDir, encodedPath)
		sessionFiles, _ := os.ReadDir(sessionDir)

		for _, sf := range sessionFiles {
			if !strings.HasSuffix(sf.Name(), ".jsonl") {
				continue
			}

			sessionID := strings.TrimSuffix(sf.Name(), ".jsonl")
			info, _ := sf.Info()

			sess, exists := sessionsMap[sessionID]
			if !exists {
				sess = &Session{
					ID:        sessionID,
					Directory: decodedPath,
				}
			}

			if info != nil {
				sess.SizeBytes = info.Size()
				if sess.Timestamp.IsZero() {
					sess.Timestamp = info.ModTime()
				}
			}

			sessionPath := filepath.Join(sessionDir, sf.Name())
			if msgCount, firstPreview, lastPreview := CountMessages(sessionPath); msgCount > 0 {
				sess.MessageCount = msgCount
				sess.Preview = firstPreview
				sess.LastPreview = lastPreview
			}

			sessions = append(sessions, *sess)
		}
	}

	sort.Slice(sessions, func(i, j int) bool {
		return sessions[i].Timestamp.After(sessions[j].Timestamp)
	})

	return sessions, nil
}

func extractTextContent(content interface{}) string {
	if text, ok := content.(string); ok {
		return text
	}

	if arr, ok := content.([]interface{}); ok {
		for _, item := range arr {
			if obj, ok := item.(map[string]interface{}); ok {
				if objType, _ := obj["type"].(string); objType == "text" {
					if text, ok := obj["text"].(string); ok {
						return text
					}
				}
			}
		}
	}

	return ""
}

func CountMessages(path string) (int, string, string) {
	f, err := os.Open(path)
	if err != nil {
		return 0, "", ""
	}
	defer f.Close()

	count := 0
	firstPreview := ""
	lastPreview := ""
	decoder := json.NewDecoder(f)

	for {
		var msg map[string]interface{}
		if err := decoder.Decode(&msg); err != nil {
			break
		}
		count++

		if msgType, ok := msg["type"].(string); ok && msgType == "user" {
			if content, ok := msg["message"].(map[string]interface{}); ok {
				text := extractTextContent(content["content"])
				if text != "" {
					if firstPreview == "" {
						if len(text) > 60 {
							firstPreview = text[:57] + "..."
						} else {
							firstPreview = text
						}
					}
					if len(text) > 60 {
						lastPreview = text[:57] + "..."
					} else {
						lastPreview = text
					}
				}
			}
		}
	}

	return count, firstPreview, lastPreview
}

type Message struct {
	Type      string
	Content   string
	Timestamp string
}

func GetLastMessages(path string, n int) []Message {
	f, err := os.Open(path)
	if err != nil {
		return nil
	}
	defer f.Close()

	var messages []Message
	decoder := json.NewDecoder(f)

	for {
		var msg map[string]interface{}
		if err := decoder.Decode(&msg); err != nil {
			break
		}

		msgType, _ := msg["type"].(string)
		if msgType != "user" && msgType != "assistant" {
			continue
		}

		content := ""
		if msgContent, ok := msg["message"].(map[string]interface{}); ok {
			content = extractTextContent(msgContent["content"])
		}

		timestamp := ""
		if ts, ok := msg["timestamp"].(string); ok {
			timestamp = ts
		}

		if content != "" {
			messages = append(messages, Message{
				Type:      msgType,
				Content:   content,
				Timestamp: timestamp,
			})
		}
	}

	if len(messages) <= n {
		return messages
	}
	return messages[len(messages)-n:]
}
