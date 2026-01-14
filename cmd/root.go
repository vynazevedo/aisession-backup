package cmd

import (
	"fmt"
	"strings"
	"time"

	"claude-migrate/internal/backup"
	"claude-migrate/internal/restore"
	"claude-migrate/internal/session"
	"claude-migrate/internal/tui"
	"claude-migrate/internal/utils"

	"github.com/spf13/cobra"
)

var Version = "dev"

func Execute() error {
	var rootCmd = &cobra.Command{
		Use:   "claude-migrate",
		Short: "Migrate Claude Code sessions between machines",
		Long: `Claude Code Session Migrator

Backup and restore Claude Code chat sessions between different machines.
Supports path remapping for WSL -> macOS or any directory structure change.`,
		PersistentPreRun: func(cmd *cobra.Command, args []string) {
			v, _ := cmd.Flags().GetBool("verbose")
			utils.Verbose = v
		},
	}
	rootCmd.PersistentFlags().BoolP("verbose", "v", false, "Enable verbose output")

	var versionCmd = &cobra.Command{
		Use:   "version",
		Short: "Print version information",
		Run: func(cmd *cobra.Command, args []string) {
			fmt.Printf("claude-migrate %s\n", Version)
		},
	}

	var outputPath string
	var backupCmd = &cobra.Command{
		Use:   "backup",
		Short: "Interactively select and backup sessions",
		Long:  "Opens an interactive UI to select which sessions to backup.",
		RunE: func(cmd *cobra.Command, args []string) error {
			claudeDir := utils.ExpandPath("~/.claude")
			if outputPath == "" {
				outputPath = fmt.Sprintf("claude-backup-%s.tar.gz", time.Now().Format("20060102-150405"))
			}
			return tui.RunInteractiveBackup(claudeDir, outputPath)
		},
	}
	backupCmd.Flags().StringVarP(&outputPath, "output", "o", "", "Output file path (default: claude-backup-<timestamp>.tar.gz)")

	var destDir string
	var mappingsStr string
	var dryRun bool
	var force bool
	var restoreCmd = &cobra.Command{
		Use:   "restore <backup-file>",
		Short: "Restore sessions from backup",
		Long:  "Restore sessions from a backup file with optional path remapping.",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			backupPath := args[0]
			claudeDir := utils.ExpandPath(destDir)

			if mappingsStr != "" {
				for _, m := range strings.Split(mappingsStr, ",") {
					if err := utils.ValidatePathMapping(m); err != nil {
						return err
					}
				}

				pathMappings := make(map[string]string)
				for _, m := range strings.Split(mappingsStr, ",") {
					parts := strings.SplitN(m, ":", 2)
					if len(parts) == 2 {
						pathMappings[parts[0]] = parts[1]
					}
				}
				return restore.Restore(backupPath, claudeDir, pathMappings, dryRun)
			}

			if force {
				return restore.Restore(backupPath, claudeDir, make(map[string]string), dryRun)
			}

			return restore.RunInteractive(backupPath, claudeDir, dryRun)
		},
	}
	restoreCmd.Flags().StringVarP(&destDir, "dest", "d", "~/.claude", "Destination Claude directory")
	restoreCmd.Flags().StringVarP(&mappingsStr, "mappings", "m", "", "Path mappings (old1:new1,old2:new2)")
	restoreCmd.Flags().BoolVar(&dryRun, "dry-run", false, "Show what would be restored without making changes")
	restoreCmd.Flags().BoolVarP(&force, "force", "f", false, "Skip interactive prompts")

	var listCmd = &cobra.Command{
		Use:   "list [backup-file]",
		Short: "List sessions in Claude directory or backup file",
		RunE: func(cmd *cobra.Command, args []string) error {
			if len(args) > 0 {
				return backup.List(args[0])
			}

			claudeDir := utils.ExpandPath("~/.claude")
			sessions, err := session.LoadSessions(claudeDir)
			if err != nil {
				return err
			}

			fmt.Printf("Found %d sessions in %s:\n\n", len(sessions), claudeDir)
			for _, sess := range sessions {
				fmt.Printf("  %s\n", sess.Directory)
				fmt.Printf("     ID: %s\n", sess.ID)
				fmt.Printf("     Messages: %d | Size: %s | Date: %s\n",
					sess.MessageCount,
					utils.HumanSize(sess.SizeBytes),
					sess.Timestamp.Format("2006-01-02 15:04"))
				if sess.Preview != "" {
					fmt.Printf("     First: %s\n", sess.Preview)
				}
				if sess.LastPreview != "" && sess.LastPreview != sess.Preview {
					fmt.Printf("     Last:  %s\n", sess.LastPreview)
				}
				fmt.Println()
			}
			return nil
		},
	}

	rootCmd.AddCommand(versionCmd, backupCmd, restoreCmd, listCmd)

	return rootCmd.Execute()
}
