import type { ReactNode } from 'react';
import { Navigate, Route, Routes, useLocation } from 'react-router';
import { isAdmin } from './api';
import { Layout } from './components/Layout';
import { useAuth } from './lib/auth';
import { AuditPage } from './pages/Audit';
import { ConnectorsPage } from './pages/Connectors';
import { DashboardPage } from './pages/Dashboard';
import { MetricsPage } from './pages/Metrics';
import { ProjectEditPage } from './pages/ProjectEdit';
import { ProjectsPage } from './pages/Projects';
import { RunPage } from './pages/Run';
import { SettingsPage } from './pages/Settings';
import { SignInPage } from './pages/SignIn';
import { UsersPage } from './pages/Users';

function RequireAuth({ children, admin }: { children: ReactNode; admin?: boolean }) {
  const { user, loading } = useAuth();
  const loc = useLocation();
  if (loading) return <div className="center-fill muted">Loading…</div>;
  if (!user) {
    const next = encodeURIComponent(loc.pathname + loc.search);
    return <Navigate to={`/signin?next=${next}`} replace />;
  }
  if (admin && !isAdmin(user)) {
    return (
      <div className="page">
        <div className="banner banner-error">This screen needs the administer role.</div>
      </div>
    );
  }
  return <>{children}</>;
}

export function App() {
  return (
    <Routes>
      <Route path="/signin" element={<SignInPage />} />
      <Route
        element={
          <RequireAuth>
            <Layout />
          </RequireAuth>
        }
      >
        <Route index element={<DashboardPage />} />
        <Route path="runs/:id" element={<RunPage />} />
        <Route path="admin/connectors" element={<RequireAuth admin><ConnectorsPage /></RequireAuth>} />
        <Route path="admin/projects" element={<RequireAuth admin><ProjectsPage /></RequireAuth>} />
        <Route path="admin/projects/:id" element={<RequireAuth admin><ProjectEditPage /></RequireAuth>} />
        <Route path="admin/users" element={<RequireAuth admin><UsersPage /></RequireAuth>} />
        <Route path="admin/settings" element={<RequireAuth admin><SettingsPage /></RequireAuth>} />
        <Route path="admin/audit" element={<RequireAuth admin><AuditPage /></RequireAuth>} />
        <Route path="admin/metrics" element={<RequireAuth admin><MetricsPage /></RequireAuth>} />
        <Route path="*" element={<div className="page muted">Not found.</div>} />
      </Route>
    </Routes>
  );
}
