package lifecycle

import (
	"context"
	"fmt"

	"github.com/abbyfluoroethane/bento/internal/hypervisor"
	"github.com/abbyfluoroethane/bento/internal/types"
)

// ReconcileReport is the disagreement between libvirt and the database
// (SPEC 6.1). The operator reads it and corrects by hand; Bento does not.
type ReconcileReport struct {
	// DomainsWithoutRows are libvirt domains with no instances row.
	DomainsWithoutRows []hypervisor.DomainInfo
	// RowsWithoutDomains are instances rows with no libvirt domain.
	RowsWithoutDomains []types.Instance
}

// Empty reports agreement: every domain has a row and every row a domain.
func (r ReconcileReport) Empty() bool {
	return len(r.DomainsWithoutRows) == 0 && len(r.RowsWithoutDomains) == 0
}

// Reconcile compares every libvirt domain against every instances row and
// reports the differences, matched by UUID (SPEC 6.1).
//
// Reconcile reports and never deletes. It performs no write of any kind: a
// reconciliation bug that deletes a domain is worse than a row that is
// wrong.
func (m *Manager) Reconcile(ctx context.Context) (ReconcileReport, error) {
	var report ReconcileReport
	domains, err := m.hyp.List(ctx)
	if err != nil {
		return report, fmt.Errorf("lifecycle: reconcile: list domains: %w", err)
	}
	rows, err := m.store.Instances()
	if err != nil {
		return report, fmt.Errorf("lifecycle: reconcile: list instances: %w", err)
	}
	byUUID := make(map[string]bool, len(rows))
	for _, inst := range rows {
		byUUID[inst.UUID] = true
	}
	domainUUIDs := make(map[string]bool, len(domains))
	for _, dom := range domains {
		domainUUIDs[dom.UUID] = true
		if !byUUID[dom.UUID] {
			report.DomainsWithoutRows = append(report.DomainsWithoutRows, dom)
		}
	}
	for _, inst := range rows {
		if !domainUUIDs[inst.UUID] {
			report.RowsWithoutDomains = append(report.RowsWithoutDomains, inst)
		}
	}
	return report, nil
}
