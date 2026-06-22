// identity.js — dev identity used to label requests in v1.
// In v2+ this becomes the seam for attaching real bearer tokens.
export const DEV_IDENTITY = {
  subject: 'dev@local',
  tenant: 'default',
};
