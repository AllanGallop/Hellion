package main

import (
	"context"
	"encoding/json"
	"fmt"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

type Store struct {
	pool *pgxpool.Pool
}

type RunRecord struct {
	RunID     string
	Status    string
	Outcome   string
	ScopeID   string
	TestPack  string
	CreatedAt time.Time
}

func NewStore(ctx context.Context, databaseURL string) (*Store, error) {
	pool, err := pgxpool.New(ctx, databaseURL)
	if err != nil {
		return nil, err
	}

	store := &Store{pool: pool}
	if err := store.migrate(ctx); err != nil {
		pool.Close()
		return nil, err
	}

	return store, nil
}

func (s *Store) Close() {
	s.pool.Close()
}

func (s *Store) migrate(ctx context.Context) error {
	_, err := s.pool.Exec(ctx, `
		CREATE TABLE IF NOT EXISTS runs (
			run_id TEXT PRIMARY KEY,
			status TEXT NOT NULL DEFAULT 'queued',
			outcome TEXT NOT NULL DEFAULT 'unknown',
			scope_id TEXT NOT NULL,
			test_pack TEXT NOT NULL,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
		);

		CREATE TABLE IF NOT EXISTS events (
			id BIGSERIAL PRIMARY KEY,
			run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
			event_type TEXT NOT NULL,
			target TEXT,
			message TEXT NOT NULL,
			severity TEXT,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
		);

		CREATE INDEX IF NOT EXISTS events_run_id_id_idx ON events (run_id, id);
	`)
	return err
}

func (s *Store) InsertRun(ctx context.Context, run RunRecord) error {
	_, err := s.pool.Exec(ctx, `
		INSERT INTO runs (run_id, status, outcome, scope_id, test_pack, created_at)
		VALUES ($1, $2, $3, $4, $5, $6)
	`, run.RunID, run.Status, run.Outcome, run.ScopeID, run.TestPack, run.CreatedAt)
	return err
}

func (s *Store) InsertRuns(ctx context.Context, runs []RunRecord) error {
	if len(runs) == 0 {
		return nil
	}

	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return err
	}
	defer tx.Rollback(ctx)

	batch := &pgx.Batch{}
	for _, run := range runs {
		batch.Queue(`
			INSERT INTO runs (run_id, status, outcome, scope_id, test_pack, created_at)
			VALUES ($1, $2, $3, $4, $5, $6)
		`, run.RunID, run.Status, run.Outcome, run.ScopeID, run.TestPack, run.CreatedAt)
	}

	br := tx.SendBatch(ctx, batch)
	for range runs {
		if _, err := br.Exec(); err != nil {
			br.Close()
			return err
		}
	}

	if err := br.Close(); err != nil {
		return err
	}

	return tx.Commit(ctx)
}

func (s *Store) RunStats(ctx context.Context) (map[string]int, error) {
	rows, err := s.pool.Query(ctx, `
		SELECT status, COUNT(*)::int
		FROM runs
		GROUP BY status
	`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	stats := map[string]int{
		"queued":    0,
		"running":   0,
		"completed": 0,
		"cancelled": 0,
		"failed":    0,
		"total":     0,
	}

	for rows.Next() {
		var status string
		var count int

		if err := rows.Scan(&status, &count); err != nil {
			return nil, err
		}

		stats[status] = count
		stats["total"] += count
	}

	return stats, rows.Err()
}

func (s *Store) ListRuns(ctx context.Context) ([]map[string]string, error) {
	rows, err := s.pool.Query(ctx, `
		SELECT run_id, status, outcome, scope_id, test_pack, created_at
		FROM runs
		ORDER BY created_at DESC
	`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	runs := []map[string]string{}

	for rows.Next() {
		var run RunRecord
		if err := rows.Scan(
			&run.RunID,
			&run.Status,
			&run.Outcome,
			&run.ScopeID,
			&run.TestPack,
			&run.CreatedAt,
		); err != nil {
			return nil, err
		}

		runs = append(runs, runToMap(run))
	}

	return runs, rows.Err()
}

func (s *Store) GetRun(ctx context.Context, runID string) (map[string]string, error) {
	var run RunRecord
	err := s.pool.QueryRow(ctx, `
		SELECT run_id, status, outcome, scope_id, test_pack, created_at
		FROM runs
		WHERE run_id = $1
	`, runID).Scan(
		&run.RunID,
		&run.Status,
		&run.Outcome,
		&run.ScopeID,
		&run.TestPack,
		&run.CreatedAt,
	)
	if err == pgx.ErrNoRows {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}

	result := runToMap(run)
	return result, nil
}

func (s *Store) RunExists(ctx context.Context, runID string) (bool, error) {
	var exists bool
	err := s.pool.QueryRow(ctx, `
		SELECT EXISTS(SELECT 1 FROM runs WHERE run_id = $1)
	`, runID).Scan(&exists)
	return exists, err
}

func (s *Store) ListRunEvents(ctx context.Context, runID string) ([]string, error) {
	rows, err := s.pool.Query(ctx, `
		SELECT event_type, run_id, target, message, severity
		FROM events
		WHERE run_id = $1
		ORDER BY id ASC
	`, runID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	lines := []string{}

	for rows.Next() {
		var eventType, runIDValue, message string
		var target, severity *string

		if err := rows.Scan(&eventType, &runIDValue, &target, &message, &severity); err != nil {
			return nil, err
		}

		payload := map[string]interface{}{
			"event_type": eventType,
			"run_id":     runIDValue,
			"message":    message,
		}

		if target != nil {
			payload["target"] = *target
		} else {
			payload["target"] = nil
		}

		if severity != nil {
			payload["severity"] = *severity
		} else {
			payload["severity"] = nil
		}

		encoded, err := json.Marshal(payload)
		if err != nil {
			return nil, err
		}

		lines = append(lines, string(encoded))
	}

	return lines, rows.Err()
}

func (s *Store) CancelRun(ctx context.Context, runID string) error {
	tag, err := s.pool.Exec(ctx, `
		UPDATE runs SET status = 'cancelled' WHERE run_id = $1
	`, runID)
	if err != nil {
		return err
	}
	if tag.RowsAffected() == 0 {
		return pgx.ErrNoRows
	}
	return nil
}

func (s *Store) DeleteRun(ctx context.Context, runID string) error {
	tag, err := s.pool.Exec(ctx, `DELETE FROM runs WHERE run_id = $1`, runID)
	if err != nil {
		return err
	}
	if tag.RowsAffected() == 0 {
		return pgx.ErrNoRows
	}
	return nil
}

func (s *Store) ClearRuns(ctx context.Context) error {
	_, err := s.pool.Exec(ctx, `TRUNCATE TABLE events, runs`)
	return err
}

func runToMap(run RunRecord) map[string]string {
	return map[string]string{
		"run_id":     run.RunID,
		"status":     run.Status,
		"outcome":    run.Outcome,
		"scope_id":   run.ScopeID,
		"test_pack":  run.TestPack,
		"created_at": run.CreatedAt.Format(time.RFC3339),
	}
}

func waitForPostgres(ctx context.Context, databaseURL string) (*Store, error) {
	var store *Store
	var err error

	for i := 0; i < 30; i++ {
		store, err = NewStore(ctx, databaseURL)
		if err == nil {
			return store, nil
		}

		time.Sleep(1 * time.Second)
	}

	return nil, fmt.Errorf("failed to connect to postgres: %w", err)
}
