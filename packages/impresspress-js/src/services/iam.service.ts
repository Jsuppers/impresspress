import { BaseService } from "./base.service";
import { IAMRole } from "../types";

/**
 * `GET /b/admin/api/iam/roles` response — `AdminRoleListResponse` on the
 * server. The endpoint takes no query parameters and does not paginate:
 * `page` is always 1 and `page_size` the handler's fixed ceiling.
 */
export interface IAMRoleListResponse {
  records: IAMRole[];
  total_count: number;
  page: number;
  page_size: number;
}

/** `POST /b/admin/api/iam/roles` body — `CreateRoleRequest` on the server. */
export interface CreateRoleRequest {
  name: string;
  description?: string;
  permissions?: string[];
}

/**
 * `PATCH /b/admin/api/iam/roles/{id}` body — `UpdateRoleRequest` on the
 * server. Only the fields present are applied; `name` is refused on a
 * system role.
 */
export interface UpdateRoleRequest {
  name?: string;
  description?: string;
  permissions?: string[];
}

export class IAMService extends BaseService {
  /**
   * List every role, sorted by name. Unwraps the server's
   * `{ records, total_count, page, page_size }` envelope — the endpoint does
   * not paginate, so the envelope carries nothing a caller needs.
   */
  async getRoles(): Promise<IAMRole[]> {
    const res = await this.request<IAMRoleListResponse>({
      method: "GET",
      url: "/b/admin/api/iam/roles",
    });
    return res.records;
  }

  /** Create a role. The response is the same row projection `getRoles` lists. */
  async createRole(role: CreateRoleRequest): Promise<IAMRole> {
    return this.request<IAMRole>({
      method: "POST",
      url: "/b/admin/api/iam/roles",
      data: role,
    });
  }

  /**
   * Update a role by its `id` (as returned by `getRoles` / `createRole`),
   * not by name — the route is keyed by row id.
   */
  async updateRole(roleId: string, updates: UpdateRoleRequest): Promise<IAMRole> {
    return this.request<IAMRole>({
      method: "PATCH",
      url: `/b/admin/api/iam/roles/${roleId}`,
      data: updates,
    });
  }

  /** Delete a role by its `id`. System roles are refused server-side. */
  async deleteRole(roleId: string): Promise<void> {
    await this.request<{ deleted: boolean }>({
      method: "DELETE",
      url: `/b/admin/api/iam/roles/${roleId}`,
    });
  }
}
