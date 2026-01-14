package tui

import (
	"fmt"

	"claude-migrate/internal/backup"
	"claude-migrate/internal/session"
	"claude-migrate/internal/utils"

	"github.com/charmbracelet/bubbles/key"
	"github.com/charmbracelet/bubbles/list"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

var (
	titleStyle        = lipgloss.NewStyle().MarginLeft(2).Bold(true).Foreground(lipgloss.Color("170"))
	itemStyle         = lipgloss.NewStyle().PaddingLeft(4)
	selectedItemStyle = lipgloss.NewStyle().PaddingLeft(2).Foreground(lipgloss.Color("170"))
	paginationStyle   = list.DefaultStyles().PaginationStyle.PaddingLeft(4)
	helpStyle         = list.DefaultStyles().HelpStyle.PaddingLeft(4).PaddingBottom(1)
	checkMark         = lipgloss.NewStyle().Foreground(lipgloss.Color("42")).SetString("[x]")
	uncheckMark       = lipgloss.NewStyle().Foreground(lipgloss.Color("240")).SetString("[ ]")
)

type SessionItem struct {
	Session  session.Session
	Selected bool
}

func (i SessionItem) Title() string {
	mark := uncheckMark.String()
	if i.Selected {
		mark = checkMark.String()
	}
	dir := i.Session.Directory
	if len(dir) > 50 {
		dir = "..." + dir[len(dir)-47:]
	}
	return fmt.Sprintf("%s %s", mark, dir)
}

func (i SessionItem) Description() string {
	desc := fmt.Sprintf("   %s | %d msgs | %s | %s",
		i.Session.ID[:8],
		i.Session.MessageCount,
		utils.HumanSize(i.Session.SizeBytes),
		i.Session.Timestamp.Format("2006-01-02 15:04"))
	if i.Session.LastPreview != "" {
		desc += fmt.Sprintf("\n   > %s", i.Session.LastPreview)
	}
	return desc
}

func (i SessionItem) FilterValue() string {
	return i.Session.Directory + " " + i.Session.ID
}

type Model struct {
	list       list.Model
	selected   map[string]bool
	claudeDir  string
	outputPath string
	quitting   bool
	done       bool
}

type keyMap struct {
	Toggle key.Binding
	All    key.Binding
	None   key.Binding
	Backup key.Binding
	Quit   key.Binding
}

var keys = keyMap{
	Toggle: key.NewBinding(
		key.WithKeys(" ", "enter"),
		key.WithHelp("space/enter", "toggle"),
	),
	All: key.NewBinding(
		key.WithKeys("a"),
		key.WithHelp("a", "select all"),
	),
	None: key.NewBinding(
		key.WithKeys("n"),
		key.WithHelp("n", "select none"),
	),
	Backup: key.NewBinding(
		key.WithKeys("b"),
		key.WithHelp("b", "backup selected"),
	),
	Quit: key.NewBinding(
		key.WithKeys("q", "ctrl+c"),
		key.WithHelp("q", "quit"),
	),
}

func (m Model) Init() tea.Cmd {
	return nil
}

func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.list.SetWidth(msg.Width)
		m.list.SetHeight(msg.Height - 4)
		return m, nil

	case tea.KeyMsg:
		switch {
		case key.Matches(msg, keys.Quit):
			m.quitting = true
			return m, tea.Quit

		case key.Matches(msg, keys.Toggle):
			if item, ok := m.list.SelectedItem().(SessionItem); ok {
				m.selected[item.Session.ID] = !m.selected[item.Session.ID]
				m.updateListItems()
			}
			return m, nil

		case key.Matches(msg, keys.All):
			for _, item := range m.list.Items() {
				if si, ok := item.(SessionItem); ok {
					m.selected[si.Session.ID] = true
				}
			}
			m.updateListItems()
			return m, nil

		case key.Matches(msg, keys.None):
			m.selected = make(map[string]bool)
			m.updateListItems()
			return m, nil

		case key.Matches(msg, keys.Backup):
			count := 0
			for _, v := range m.selected {
				if v {
					count++
				}
			}
			if count == 0 {
				return m, nil
			}
			m.done = true
			return m, tea.Quit
		}
	}

	var cmd tea.Cmd
	m.list, cmd = m.list.Update(msg)
	return m, cmd
}

func (m *Model) updateListItems() {
	items := m.list.Items()
	newItems := make([]list.Item, len(items))
	for i, item := range items {
		if si, ok := item.(SessionItem); ok {
			si.Selected = m.selected[si.Session.ID]
			newItems[i] = si
		}
	}
	m.list.SetItems(newItems)
}

func (m Model) View() string {
	if m.quitting {
		return "Cancelled.\n"
	}
	if m.done {
		count := 0
		for _, v := range m.selected {
			if v {
				count++
			}
		}
		return fmt.Sprintf("Creating backup of %d sessions...\n", count)
	}

	count := 0
	for _, v := range m.selected {
		if v {
			count++
		}
	}

	header := titleStyle.Render(fmt.Sprintf("Claude Code Sessions (%d selected)", count))
	help := helpStyle.Render("space: toggle | a: all | n: none | b: backup | q: quit")

	return fmt.Sprintf("%s\n%s\n%s", header, m.list.View(), help)
}

func RunInteractiveBackup(claudeDir, outputPath string) error {
	sessions, err := session.LoadSessions(claudeDir)
	if err != nil {
		return err
	}

	if len(sessions) == 0 {
		return fmt.Errorf("no sessions found in %s", claudeDir)
	}

	items := make([]list.Item, len(sessions))
	for i, sess := range sessions {
		items[i] = SessionItem{Session: sess, Selected: false}
	}

	delegate := list.NewDefaultDelegate()
	delegate.Styles.SelectedTitle = selectedItemStyle
	delegate.Styles.NormalTitle = itemStyle

	l := list.New(items, delegate, 80, 20)
	l.Title = "Select sessions to backup"
	l.SetShowStatusBar(false)
	l.SetFilteringEnabled(true)
	l.Styles.Title = titleStyle
	l.Styles.PaginationStyle = paginationStyle
	l.Styles.HelpStyle = helpStyle

	m := Model{
		list:       l,
		selected:   make(map[string]bool),
		claudeDir:  claudeDir,
		outputPath: outputPath,
	}

	p := tea.NewProgram(m, tea.WithAltScreen())
	finalModel, err := p.Run()
	if err != nil {
		return err
	}

	fm := finalModel.(Model)
	if fm.quitting || !fm.done {
		return nil
	}

	var selectedSessions []session.Session
	for _, sess := range sessions {
		if fm.selected[sess.ID] {
			selectedSessions = append(selectedSessions, sess)
		}
	}

	if len(selectedSessions) == 0 {
		fmt.Println("No sessions selected.")
		return nil
	}

	var totalSize int64
	for _, sess := range selectedSessions {
		totalSize += sess.SizeBytes
	}

	fmt.Printf("\nCreating backup of %d sessions (%s) to %s...\n", len(selectedSessions), utils.HumanSize(totalSize), outputPath)
	if err := backup.Create(claudeDir, selectedSessions, outputPath); err != nil {
		return err
	}

	fmt.Printf("Backup created: %s\n", outputPath)
	return nil
}
