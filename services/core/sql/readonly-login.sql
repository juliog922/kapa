-- =============================================================================
--  Kapa core: a READ-ONLY login for the Sage database (run once, as a Sage DBA)
--
--  core refuses any login that can write (see `core --check-sqlserver`). This login
--  can only SELECT the four tables core reads, and is explicitly DENIED every write,
--  DDL and EXECUTE permission at database level (DENY overrides any role grant).
--  Replace <strong password>, then put kapa_ro in services/core/secrets/sqlserver.*.env.
-- =============================================================================
USE [master];
CREATE LOGIN [kapa_ro] WITH PASSWORD = N'<strong password>', CHECK_POLICY = ON, DEFAULT_DATABASE = [Sage];
GO

USE [Sage];
CREATE USER [kapa_ro] FOR LOGIN [kapa_ro];

GRANT SELECT ON [dbo].[Clientes]              TO [kapa_ro];
GRANT SELECT ON [dbo].[Articulos]             TO [kapa_ro];
GRANT SELECT ON [dbo].[CabeceraPedidoCliente] TO [kapa_ro];
GRANT SELECT ON [dbo].[LineasPedidoCliente]   TO [kapa_ro];

DENY INSERT, UPDATE, DELETE, EXECUTE, ALTER, CONTROL, TAKE OWNERSHIP,
     CREATE TABLE, CREATE PROCEDURE, CREATE FUNCTION, CREATE VIEW, CREATE SCHEMA
     TO [kapa_ro];
GO

-- Verify (as kapa_ro): must return no rows.
-- SELECT permission_name FROM fn_my_permissions(NULL, 'DATABASE')
--  WHERE permission_name IN ('INSERT', 'UPDATE', 'DELETE', 'ALTER', 'CONTROL', 'EXECUTE');
