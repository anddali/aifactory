import { useState } from 'react';
import { NavLink, Outlet, useNavigate } from 'react-router';
import { isAdmin } from '../api';
import { useAuth } from '../lib/auth';

const ADMIN_LINKS: [string, string][] = [
  ['/admin/connectors', 'Connectors'],
  ['/admin/projects', 'Projects'],
  ['/admin/users', 'Users'],
  ['/admin/settings', 'Settings'],
  ['/admin/audit', 'Audit'],
  ['/admin/metrics', 'Outcomes'],
];

export function Layout() {
  const { user, signOut } = useAuth();
  const navigate = useNavigate();
  const [menuOpen, setMenuOpen] = useState(false);
  const admin = isAdmin(user);

  return (
    <div className="app">
      <header className="topbar">
        <NavLink to="/" className="brand" onClick={() => setMenuOpen(false)}>
          <span className="brand-mark">◧</span> AI Factory
        </NavLink>
        <button className="icon-btn nav-toggle" aria-label="Menu" aria-expanded={menuOpen} onClick={() => setMenuOpen((o) => !o)}>
          ☰
        </button>
        <nav className={`nav${menuOpen ? ' open' : ''}`} onClick={() => setMenuOpen(false)}>
          <NavLink to="/" end>
            Runs
          </NavLink>
          {admin ? ADMIN_LINKS.map(([to, label]) => <NavLink key={to} to={to}>{label}</NavLink>) : null}
        </nav>
        <div className="userbox">
          {user ? (
            <>
              <span className="user-name" title={user.email}>
                {user.display_name || user.email}
                {user.is_global_admin ? <span className="tag">admin</span> : null}
              </span>
              <button
                className="ghost small"
                onClick={async () => {
                  await signOut();
                  navigate('/signin');
                }}
              >
                Sign out
              </button>
            </>
          ) : null}
        </div>
      </header>
      <main className="main">
        <Outlet />
      </main>
    </div>
  );
}
