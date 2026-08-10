package store

import (
	"database/sql"
	"errors"
	"time"

	"github.com/abbyfluoroethane/bento/internal/types"
)

const instanceColumns = `uuid, name, owner_id, host_id, image_name, base_checksum,
	state, desired_state, address, mac, vcpu, memory, disk, nested, ksm,
	http_port, visibility, created_at, last_seen_at`

// CreateInstance inserts an instance after running the name cooldown check
// (SPEC 7.2) and the four-limit quota check (SPEC 6.1) in the same
// transaction as the insert. Two concurrent creates therefore cannot both
// pass a check when only one instance fits. A user with no quota row is
// unlimited.
func (s *Store) CreateInstance(inst types.Instance, nameCooldown time.Duration) error {
	return s.inTx(func(tx *sql.Tx) error {
		if err := s.claimNameTx(tx, inst.Name, inst.OwnerID, nameCooldown); err != nil {
			return err
		}
		if err := checkQuotaTx(tx, inst.OwnerID, "", 1,
			int64(inst.VCPU), inst.MemoryMiB, inst.DiskGiB); err != nil {
			return err
		}
		now := s.now()
		created := inst.CreatedAt
		if created.IsZero() {
			created = now
		}
		_, err := tx.Exec(`INSERT INTO instances
			(uuid, name, owner_id, host_id, image_name, base_checksum,
			 state, desired_state, address, mac, vcpu, memory, disk,
			 nested, ksm, http_port, visibility, created_at)
			VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
			inst.UUID, inst.Name, inst.OwnerID, inst.HostID, inst.ImageName,
			inst.BaseChecksum, string(inst.State), string(inst.DesiredState),
			inst.Address, inst.MAC, inst.VCPU, inst.MemoryMiB, inst.DiskGiB,
			inst.Nested, inst.KSM, inst.HTTPPort, string(inst.Visibility),
			fmtTime(created))
		return err
	})
}

// checkQuotaTx runs the four-limit check (SPEC 6.1) against the sum of the
// owner's instances, excluding excludeUUID (empty for a create), plus the
// requested resources.
func checkQuotaTx(tx *sql.Tx, ownerID int64, excludeUUID string,
	addInstances, addVCPU, addMemory, addDisk int64) error {
	var q types.Quota
	err := tx.QueryRow(`SELECT max_instances, max_vcpu, max_memory, max_disk
		FROM quotas WHERE user_id = ?`, ownerID).
		Scan(&q.MaxInstances, &q.MaxVCPU, &q.MaxMemoryMiB, &q.MaxDiskGiB)
	if errors.Is(err, sql.ErrNoRows) {
		return nil // no quota row means unlimited
	}
	if err != nil {
		return err
	}

	var count, vcpu, memory, disk int64
	err = tx.QueryRow(`SELECT COUNT(*), COALESCE(SUM(vcpu), 0),
		COALESCE(SUM(memory), 0), COALESCE(SUM(disk), 0)
		FROM instances WHERE owner_id = ? AND uuid != ?`, ownerID, excludeUUID).
		Scan(&count, &vcpu, &memory, &disk)
	if err != nil {
		return err
	}

	for _, check := range []struct {
		limit          string
		used, add, max int64
	}{
		{"instances", count, addInstances, int64(q.MaxInstances)},
		{"vcpu", vcpu, addVCPU, int64(q.MaxVCPU)},
		{"memory", memory, addMemory, q.MaxMemoryMiB},
		{"disk", disk, addDisk, q.MaxDiskGiB},
	} {
		if check.used+check.add > check.max {
			return &QuotaError{
				Limit:     check.limit,
				Used:      check.used,
				Requested: check.add,
				Max:       check.max,
			}
		}
	}
	return nil
}

// DeleteInstance removes the row (its shares cascade with it, SPEC 7.2)
// and releases the name into the cooldown, in one transaction. It returns
// the deleted instance so the caller can clean up the domain and overlay.
func (s *Store) DeleteInstance(uuid string) (types.Instance, error) {
	var inst types.Instance
	err := s.inTx(func(tx *sql.Tx) error {
		var err error
		inst, err = getInstanceTx(tx, `uuid = ?`, uuid)
		if err != nil {
			return err
		}
		if _, err := tx.Exec(`DELETE FROM instances WHERE uuid = ?`, uuid); err != nil {
			return err
		}
		return s.releaseNameTx(tx, inst.Name, inst.OwnerID)
	})
	return inst, err
}

// RenameInstance changes the label of an instance (SPEC 7.3). The claim
// check on the new name, the update, and the release of the old name run
// in one transaction. Bento never redirects the old name.
func (s *Store) RenameInstance(uuid, newName string, nameCooldown time.Duration) error {
	return s.inTx(func(tx *sql.Tx) error {
		inst, err := getInstanceTx(tx, `uuid = ?`, uuid)
		if err != nil {
			return err
		}
		if inst.Name == newName {
			return nil
		}
		if err := s.claimNameTx(tx, newName, inst.OwnerID, nameCooldown); err != nil {
			return err
		}
		if _, err := tx.Exec(`UPDATE instances SET name = ? WHERE uuid = ?`, newName, uuid); err != nil {
			return err
		}
		return s.releaseNameTx(tx, inst.Name, inst.OwnerID)
	})
}

// Resize updates the vCPU count, memory, disk, and nested setting of an
// instance, rerunning the quota check with the instance's own use excluded
// (SPEC 6.1, 11.1).
func (s *Store) Resize(uuid string, vcpu int, memoryMiB, diskGiB int64, nested bool) error {
	return s.inTx(func(tx *sql.Tx) error {
		inst, err := getInstanceTx(tx, `uuid = ?`, uuid)
		if err != nil {
			return err
		}
		if err := checkQuotaTx(tx, inst.OwnerID, uuid, 1,
			int64(vcpu), memoryMiB, diskGiB); err != nil {
			return err
		}
		_, err = tx.Exec(`UPDATE instances SET vcpu = ?, memory = ?, disk = ?, nested = ?
			WHERE uuid = ?`, vcpu, memoryMiB, diskGiB, nested, uuid)
		return err
	})
}

// Instance returns one instance by UUID, the identifier (SPEC 7.2).
func (s *Store) Instance(uuid string) (types.Instance, error) {
	return s.getInstance(`uuid = ?`, uuid)
}

// InstanceByName returns one instance by its current label. The proxy and
// the SSH frontend resolve the name on every request (SPEC 7.1).
func (s *Store) InstanceByName(name string) (types.Instance, error) {
	return s.getInstance(`name = ?`, name)
}

// InstancesByOwner lists the instances of one user, oldest first.
func (s *Store) InstancesByOwner(ownerID int64) ([]types.Instance, error) {
	return s.listInstances(`WHERE owner_id = ? ORDER BY created_at, uuid`, ownerID)
}

// Instances lists every instance, oldest first. The reconcile report
// compares this list against the domains on the host (SPEC 6.1).
func (s *Store) Instances() ([]types.Instance, error) {
	return s.listInstances(`ORDER BY created_at, uuid`)
}

// InstancesToRestore lists instances with desired state running and
// observed state stopped: what the host reboot restore starts in batches
// (SPEC 11.2).
func (s *Store) InstancesToRestore() ([]types.Instance, error) {
	return s.listInstances(
		`WHERE desired_state = 'running' AND state = 'stopped' ORDER BY created_at, uuid`)
}

// SetDesiredState records the last user action (SPEC 11.1). Bento is
// authoritative for this column.
func (s *Store) SetDesiredState(uuid string, state types.DesiredState) error {
	return s.updateInstance(uuid, `desired_state = ?`, string(state))
}

// SetObservedState records what libvirt reports for one domain
// (SPEC 11.1). libvirt is authoritative for this column.
func (s *Store) SetObservedState(uuid string, state types.State) error {
	return s.updateInstance(uuid, `state = ?`, string(state))
}

// UpdateObservedStates applies the 30-second poll of
// virConnectListAllDomains in one transaction (SPEC 12). UUIDs with no row
// are skipped; the reconcile report covers those.
func (s *Store) UpdateObservedStates(states map[string]types.State) error {
	if len(states) == 0 {
		return nil
	}
	return s.inTx(func(tx *sql.Tx) error {
		stmt, err := tx.Prepare(`UPDATE instances SET state = ? WHERE uuid = ?`)
		if err != nil {
			return err
		}
		defer stmt.Close()
		for uuid, state := range states {
			if _, err := stmt.Exec(string(state), uuid); err != nil {
				return err
			}
		}
		return nil
	})
}

// SetVisibility sets the visibility value (SPEC 9.2).
func (s *Store) SetVisibility(uuid string, v types.Visibility) error {
	return s.updateInstance(uuid, `visibility = ?`, string(v))
}

// SetHTTPPort sets the default HTTP port the proxy targets (SPEC 9.1).
func (s *Store) SetHTTPPort(uuid string, port int) error {
	return s.updateInstance(uuid, `http_port = ?`, port)
}

// TouchLastSeen records an SSH connection or HTTP request (SPEC 12).
// Bento never acts on this column; it only feeds the `ls` output.
func (s *Store) TouchLastSeen(uuid string) error {
	return s.updateInstance(uuid, `last_seen_at = ?`, fmtTime(s.now()))
}

func (s *Store) updateInstance(uuid, set string, arg any) error {
	res, err := s.db.Exec(`UPDATE instances SET `+set+` WHERE uuid = ?`, arg, uuid)
	if err != nil {
		return err
	}
	n, err := res.RowsAffected()
	if err != nil {
		return err
	}
	if n == 0 {
		return ErrNotFound
	}
	return nil
}

func (s *Store) getInstance(where string, args ...any) (types.Instance, error) {
	row := s.db.QueryRow(
		`SELECT `+instanceColumns+` FROM instances WHERE `+where, args...)
	return scanInstance(row)
}

func getInstanceTx(tx *sql.Tx, where string, args ...any) (types.Instance, error) {
	row := tx.QueryRow(
		`SELECT `+instanceColumns+` FROM instances WHERE `+where, args...)
	return scanInstance(row)
}

func (s *Store) listInstances(tail string, args ...any) ([]types.Instance, error) {
	rows, err := s.db.Query(`SELECT `+instanceColumns+` FROM instances `+tail, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []types.Instance
	for rows.Next() {
		inst, err := scanInstance(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, inst)
	}
	return out, rows.Err()
}

type rowScanner interface {
	Scan(dest ...any) error
}

func scanInstance(row rowScanner) (types.Instance, error) {
	var (
		inst           types.Instance
		state, desired string
		visibility     string
		created        string
		lastSeen       sql.NullString
	)
	err := row.Scan(&inst.UUID, &inst.Name, &inst.OwnerID, &inst.HostID,
		&inst.ImageName, &inst.BaseChecksum, &state, &desired,
		&inst.Address, &inst.MAC, &inst.VCPU, &inst.MemoryMiB, &inst.DiskGiB,
		&inst.Nested, &inst.KSM, &inst.HTTPPort, &visibility,
		&created, &lastSeen)
	if errors.Is(err, sql.ErrNoRows) {
		return types.Instance{}, ErrNotFound
	}
	if err != nil {
		return types.Instance{}, err
	}
	inst.State = types.State(state)
	inst.DesiredState = types.DesiredState(desired)
	inst.Visibility = types.Visibility(visibility)
	if inst.CreatedAt, err = parseTime(created); err != nil {
		return types.Instance{}, err
	}
	if inst.LastSeenAt, err = parseNullTime(lastSeen); err != nil {
		return types.Instance{}, err
	}
	return inst, nil
}
