package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/google/uuid"
	"github.com/jackc/pgx/v5"
	"github.com/nats-io/nats.go"
	"gopkg.in/yaml.v3"
)

var ctx = context.Background()

type RunRequest struct {
	ScopeID  string   `json:"scope_id"`
	Targets  []string `json:"targets"`
	TestPack string   `json:"test_pack"`
}

type BulkRunRequest struct {
	ScopeID  string   `json:"scope_id"`
	Target   string   `json:"target"`
	Targets  []string `json:"targets"`
	TestPack string   `json:"test_pack"`
	Count    int      `json:"count"`
}

type Job struct {
	RunID    string `json:"run_id"`
	ScopeID  string `json:"scope_id"`
	Target   string `json:"target"`
	TestPack string `json:"test_pack"`
}

func main() {
	databaseURL := getenv("DATABASE_URL", "postgres://surface:surface@localhost:5432/surface_tester?sslmode=disable")

	store, err := waitForPostgres(ctx, databaseURL)
	if err != nil {
		log.Fatal(err)
	}
	defer store.Close()

	natsURL := getenv("NATS_URL", "nats://localhost:4222")

	var nc *nats.Conn

	for i := 0; i < 30; i++ {
		var err error

		nc, err = nats.Connect(natsURL)
		if err == nil {
			break
		}

		log.Printf("waiting for nats: %v", err)
		time.Sleep(1 * time.Second)
	}

	if nc == nil {
		log.Fatal("failed to connect to nats")
	}

	defer nc.Close()

	http.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, map[string]string{"status": "ok"})
	})

	http.HandleFunc("/runs/bulk", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			w.WriteHeader(http.StatusMethodNotAllowed)
			return
		}

		createBulkRun(w, r, store, nc)
	})

	http.HandleFunc("/runs/stats", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet {
			w.WriteHeader(http.StatusMethodNotAllowed)
			return
		}

		runStats(w, r, store)
	})

	http.HandleFunc("/runs", func(w http.ResponseWriter, r *http.Request) {
		switch r.Method {
		case http.MethodPost:
			createRun(w, r, store, nc)
		case http.MethodGet:
			listRuns(w, r, store)
		default:
			w.WriteHeader(http.StatusMethodNotAllowed)
		}
	})

	http.HandleFunc("/runs/", func(w http.ResponseWriter, r *http.Request) {
		path := strings.TrimPrefix(r.URL.Path, "/runs/")
		parts := strings.Split(path, "/")
		runID := parts[0]

		if runID == "" {
			http.NotFound(w, r)
			return
		}

		if len(parts) == 1 && r.Method == http.MethodGet {
			getRun(w, r, store, runID)
			return
		}

		if len(parts) == 2 && parts[1] == "events" && r.Method == http.MethodGet {
			getRunEvents(w, r, store, runID)
			return
		}

		if len(parts) == 2 && parts[1] == "cancel" && r.Method == http.MethodPost {
			cancelRun(w, r, store, runID)
			return
		}

		if len(parts) == 1 && r.Method == http.MethodDelete {
			deleteRun(w, r, store, runID)
			return
		}

		http.NotFound(w, r)
	})

	http.HandleFunc("/admin/queue/clear", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			w.WriteHeader(http.StatusMethodNotAllowed)
			return
		}

		writeJSON(w, map[string]string{"status": "queue cleared"})
	})

	http.HandleFunc("/admin/runs/clear", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			w.WriteHeader(http.StatusMethodNotAllowed)
			return
		}

		clearRuns(w, r, store)
	})

	log.Println("control-api listening on :8080")
	log.Fatal(http.ListenAndServe(":8080", nil))
}

func newRunID() string {
	return "run_" + uuid.New().String()
}

func createRun(w http.ResponseWriter, r *http.Request, store *Store, nc *nats.Conn) {
	var req RunRequest

	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	if req.ScopeID == "" {
		http.Error(w, "scope_id is required", http.StatusBadRequest)
		return
	}

	if req.TestPack == "" {
		http.Error(w, "test_pack is required", http.StatusBadRequest)
		return
	}

	if len(req.Targets) == 0 {
		http.Error(w, "at least one target is required", http.StatusBadRequest)
		return
	}

	if err := validateTestPack(req.TestPack); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	runID := newRunID()
	createdAt := time.Now()

	if err := store.InsertRun(ctx, RunRecord{
		RunID:     runID,
		Status:    "queued",
		Outcome:   "unknown",
		ScopeID:   req.ScopeID,
		TestPack:  req.TestPack,
		CreatedAt: createdAt,
	}); err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	for _, target := range req.Targets {
		job := Job{
			RunID:    runID,
			ScopeID:  req.ScopeID,
			Target:   target,
			TestPack: req.TestPack,
		}

		payload, _ := json.Marshal(job)

		subject := "hellion.jobs.http." + req.ScopeID

		if err := nc.Publish(subject, payload); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
	}

	nc.Flush()

	writeJSON(w, map[string]string{
		"run_id":  runID,
		"status":  "queued",
		"outcome": "unknown",
	})
}

func createBulkRun(w http.ResponseWriter, r *http.Request, store *Store, nc *nats.Conn) {
	var req BulkRunRequest

	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	if req.ScopeID == "" {
		http.Error(w, "scope_id is required", http.StatusBadRequest)
		return
	}

	if req.TestPack == "" {
		http.Error(w, "test_pack is required", http.StatusBadRequest)
		return
	}

	if err := validateTestPack(req.TestPack); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}

	targets := req.Targets

	if len(targets) == 0 {
		if req.Target == "" {
			http.Error(w, "target or targets is required", http.StatusBadRequest)
			return
		}

		count := req.Count
		if count <= 0 {
			count = 1
		}

		for i := 0; i < count; i++ {
			targets = append(targets, req.Target)
		}
	}

	subject := "hellion.jobs.http." + req.ScopeID
	createdAt := time.Now()
	runRecords := make([]RunRecord, 0, len(targets))
	jobs := make([]Job, 0, len(targets))

	for _, target := range targets {
		runID := newRunID()

		runRecords = append(runRecords, RunRecord{
			RunID:     runID,
			Status:    "queued",
			Outcome:   "unknown",
			ScopeID:   req.ScopeID,
			TestPack:  req.TestPack,
			CreatedAt: createdAt,
		})

		jobs = append(jobs, Job{
			RunID:    runID,
			ScopeID:  req.ScopeID,
			Target:   target,
			TestPack: req.TestPack,
		})
	}

	if err := store.InsertRuns(ctx, runRecords); err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	for _, job := range jobs {
		payload, _ := json.Marshal(job)

		if err := nc.Publish(subject, payload); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
	}

	if err := nc.Flush(); err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	writeJSON(w, map[string]interface{}{
		"status":  "queued",
		"created": len(jobs),
	})
}

func runStats(w http.ResponseWriter, r *http.Request, store *Store) {
	stats, err := store.RunStats(ctx)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	writeJSON(w, stats)
}

func listRuns(w http.ResponseWriter, r *http.Request, store *Store) {
	runs, err := store.ListRuns(ctx)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	writeJSON(w, runs)
}

func getRun(w http.ResponseWriter, r *http.Request, store *Store, runID string) {
	data, err := store.GetRun(ctx, runID)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	if data == nil {
		http.NotFound(w, r)
		return
	}

	writeJSON(w, data)
}

func getRunEvents(w http.ResponseWriter, r *http.Request, store *Store, runID string) {
	exists, err := store.RunExists(ctx, runID)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	if !exists {
		http.NotFound(w, r)
		return
	}

	events, err := store.ListRunEvents(ctx, runID)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "application/x-ndjson")

	for _, event := range events {
		_, _ = w.Write([]byte(event + "\n"))
	}
}

func cancelRun(w http.ResponseWriter, r *http.Request, store *Store, runID string) {
	if err := store.CancelRun(ctx, runID); err != nil {
		if errors.Is(err, pgx.ErrNoRows) {
			http.NotFound(w, r)
			return
		}

		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	writeJSON(w, map[string]string{
		"run_id": runID,
		"status": "cancelled",
	})
}

func deleteRun(w http.ResponseWriter, r *http.Request, store *Store, runID string) {
	if err := store.DeleteRun(ctx, runID); err != nil {
		if errors.Is(err, pgx.ErrNoRows) {
			http.NotFound(w, r)
			return
		}

		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	writeJSON(w, map[string]string{
		"run_id": runID,
		"status": "deleted",
	})
}

func clearRuns(w http.ResponseWriter, r *http.Request, store *Store) {
	if err := store.ClearRuns(ctx); err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}

	writeJSON(w, map[string]string{
		"status": "runs cleared",
	})
}

func writeJSON(w http.ResponseWriter, value interface{}) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(value)
}

func getenv(key, fallback string) string {
	value := os.Getenv(key)
	if value == "" {
		return fallback
	}

	return value
}

func validateTestPack(testPack string) error {
	path := "/app/test-packs/" + testPack + ".yaml"

	raw, err := os.ReadFile(path)
	if err != nil {
		return err
	}

	var parsed map[string]interface{}
	if err := yaml.Unmarshal(raw, &parsed); err != nil {
		return err
	}

	if _, ok := parsed["id"]; !ok {
		return fmt.Errorf("missing id")
	}

	if _, ok := parsed["steps"]; !ok {
		return fmt.Errorf("missing steps")
	}

	return nil
}
