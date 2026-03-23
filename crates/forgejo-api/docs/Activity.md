# Activity

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**act_user** | Option<[**models::User**](User.md)> |  | [optional]
**act_user_id** | Option<**i64**> |  | [optional]
**comment** | Option<[**models::Comment**](Comment.md)> |  | [optional]
**comment_id** | Option<**i64**> |  | [optional]
**content** | Option<**String**> |  | [optional]
**created** | Option<**String**> |  | [optional]
**id** | Option<**i64**> |  | [optional]
**is_private** | Option<**bool**> |  | [optional]
**op_type** | Option<**OpType**> | the type of action (enum: create_repo, rename_repo, star_repo, watch_repo, commit_repo, create_issue, create_pull_request, transfer_repo, push_tag, comment_issue, merge_pull_request, close_issue, reopen_issue, close_pull_request, reopen_pull_request, delete_tag, delete_branch, mirror_sync_push, mirror_sync_create, mirror_sync_delete, approve_pull_request, reject_pull_request, comment_pull, publish_release, pull_review_dismissed, pull_request_ready_for_review, auto_merge_pull_request) | [optional]
**ref_name** | Option<**String**> |  | [optional]
**repo** | Option<[**models::Repository**](Repository.md)> |  | [optional]
**repo_id** | Option<**i64**> |  | [optional]
**user_id** | Option<**i64**> |  | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


