use crate::{
  activities::{verify_is_public, verify_person_in_community},
  context::lemmy_context,
  fetcher::object_id::ObjectId,
  objects::{
    community::ApubCommunity,
    person::ApubPerson,
    tombstone::Tombstone,
    ImageObject,
    Source,
  },
};
use activitystreams::{
  base::AnyBase,
  object::kind::{ImageType, PageType},
  primitives::OneOrMany,
  public,
  unparsed::Unparsed,
};
use anyhow::anyhow;
use chrono::{DateTime, FixedOffset, NaiveDateTime};
use lemmy_api_common::blocking;
use lemmy_apub_lib::{
  traits::ApubObject,
  values::{MediaTypeHtml, MediaTypeMarkdown},
  verify::verify_domains_match,
};
use lemmy_db_schema::{
  self,
  source::{
    community::Community,
    person::Person,
    post::{Post, PostForm},
  },
  traits::Crud,
};
use lemmy_utils::{
  request::fetch_site_data,
  utils::{check_slurs, convert_datetime, markdown_to_html, remove_slurs},
  LemmyError,
};
use lemmy_websocket::LemmyContext;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::ops::Deref;
use url::Url;

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
  #[serde(rename = "@context")]
  context: OneOrMany<AnyBase>,
  r#type: PageType,
  id: Url,
  pub(crate) attributed_to: ObjectId<ApubPerson>,
  to: Vec<Url>,
  name: String,
  content: Option<String>,
  media_type: Option<MediaTypeHtml>,
  source: Option<Source>,
  url: Option<Url>,
  image: Option<ImageObject>,
  pub(crate) comments_enabled: Option<bool>,
  sensitive: Option<bool>,
  pub(crate) stickied: Option<bool>,
  published: Option<DateTime<FixedOffset>>,
  updated: Option<DateTime<FixedOffset>>,
  #[serde(flatten)]
  unparsed: Unparsed,
}

impl Page {
  pub(crate) fn id_unchecked(&self) -> &Url {
    &self.id
  }
  pub(crate) fn id(&self, expected_domain: &Url) -> Result<&Url, LemmyError> {
    verify_domains_match(&self.id, expected_domain)?;
    Ok(&self.id)
  }

  /// Only mods can change the post's stickied/locked status. So if either of these is changed from
  /// the current value, it is a mod action and needs to be verified as such.
  ///
  /// Both stickied and locked need to be false on a newly created post (verified in [[CreatePost]].
  pub(crate) async fn is_mod_action(&self, context: &LemmyContext) -> Result<bool, LemmyError> {
    let old_post = ObjectId::<ApubPost>::new(self.id.clone())
      .dereference_local(context)
      .await;

    let is_mod_action = if let Ok(old_post) = old_post {
      self.stickied != Some(old_post.stickied) || self.comments_enabled != Some(!old_post.locked)
    } else {
      false
    };
    Ok(is_mod_action)
  }

  pub(crate) async fn verify(
    &self,
    context: &LemmyContext,
    request_counter: &mut i32,
  ) -> Result<(), LemmyError> {
    let community = self.extract_community(context, request_counter).await?;

    check_slurs(&self.name, &context.settings().slur_regex())?;
    verify_domains_match(self.attributed_to.inner(), &self.id.clone())?;
    verify_person_in_community(&self.attributed_to, &community, context, request_counter).await?;
    verify_is_public(&self.to.clone())?;
    Ok(())
  }

  pub(crate) async fn extract_community(
    &self,
    context: &LemmyContext,
    request_counter: &mut i32,
  ) -> Result<ApubCommunity, LemmyError> {
    let mut to_iter = self.to.iter();
    loop {
      if let Some(cid) = to_iter.next() {
        let cid = ObjectId::new(cid.clone());
        if let Ok(c) = cid.dereference(context, request_counter).await {
          break Ok(c);
        }
      } else {
        return Err(anyhow!("No community found in cc").into());
      }
    }
  }
}

#[derive(Clone, Debug)]
pub struct ApubPost(Post);

impl Deref for ApubPost {
  type Target = Post;
  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl From<Post> for ApubPost {
  fn from(p: Post) -> Self {
    ApubPost { 0: p }
  }
}

#[async_trait::async_trait(?Send)]
impl ApubObject for ApubPost {
  type DataType = LemmyContext;
  type ApubType = Page;
  type TombstoneType = Tombstone;

  fn last_refreshed_at(&self) -> Option<NaiveDateTime> {
    None
  }

  async fn read_from_apub_id(
    object_id: Url,
    context: &LemmyContext,
  ) -> Result<Option<Self>, LemmyError> {
    Ok(
      blocking(context.pool(), move |conn| {
        Post::read_from_apub_id(conn, object_id)
      })
      .await??
      .map(Into::into),
    )
  }

  async fn delete(self, context: &LemmyContext) -> Result<(), LemmyError> {
    blocking(context.pool(), move |conn| {
      Post::update_deleted(conn, self.id, true)
    })
    .await??;
    Ok(())
  }

  // Turn a Lemmy post into an ActivityPub page that can be sent out over the network.
  async fn to_apub(&self, context: &LemmyContext) -> Result<Page, LemmyError> {
    let creator_id = self.creator_id;
    let creator = blocking(context.pool(), move |conn| Person::read(conn, creator_id)).await??;
    let community_id = self.community_id;
    let community = blocking(context.pool(), move |conn| {
      Community::read(conn, community_id)
    })
    .await??;

    let source = self.body.clone().map(|body| Source {
      content: body,
      media_type: MediaTypeMarkdown::Markdown,
    });
    let image = self.thumbnail_url.clone().map(|thumb| ImageObject {
      kind: ImageType::Image,
      url: thumb.into(),
    });

    let page = Page {
      context: lemmy_context(),
      r#type: PageType::Page,
      id: self.ap_id.clone().into(),
      attributed_to: ObjectId::new(creator.actor_id),
      to: vec![community.actor_id.into(), public()],
      name: self.name.clone(),
      content: self.body.as_ref().map(|b| markdown_to_html(b)),
      media_type: Some(MediaTypeHtml::Html),
      source,
      url: self.url.clone().map(|u| u.into()),
      image,
      comments_enabled: Some(!self.locked),
      sensitive: Some(self.nsfw),
      stickied: Some(self.stickied),
      published: Some(convert_datetime(self.published)),
      updated: self.updated.map(convert_datetime),
      unparsed: Default::default(),
    };
    Ok(page)
  }

  fn to_tombstone(&self) -> Result<Tombstone, LemmyError> {
    Ok(Tombstone::new(
      PageType::Page,
      self.updated.unwrap_or(self.published),
    ))
  }

  async fn from_apub(
    page: &Page,
    context: &LemmyContext,
    expected_domain: &Url,
    request_counter: &mut i32,
  ) -> Result<ApubPost, LemmyError> {
    // We can't verify the domain in case of mod action, because the mod may be on a different
    // instance from the post author.
    let ap_id = if page.is_mod_action(context).await? {
      page.id_unchecked()
    } else {
      page.id(expected_domain)?
    };
    let ap_id = Some(ap_id.clone().into());
    let creator = page
      .attributed_to
      .dereference(context, request_counter)
      .await?;
    let community = page.extract_community(context, request_counter).await?;
    verify_person_in_community(&page.attributed_to, &community, context, request_counter).await?;

    let thumbnail_url: Option<Url> = page.image.clone().map(|i| i.url);
    let (metadata_res, pictrs_thumbnail) = if let Some(url) = &page.url {
      fetch_site_data(context.client(), &context.settings(), Some(url)).await
    } else {
      (None, thumbnail_url)
    };
    let (embed_title, embed_description, embed_html) = metadata_res
      .map(|u| (u.title, u.description, u.html))
      .unwrap_or((None, None, None));

    let body_slurs_removed = page
      .source
      .as_ref()
      .map(|s| remove_slurs(&s.content, &context.settings().slur_regex()));
    let form = PostForm {
      name: page.name.clone(),
      url: page.url.clone().map(|u| u.into()),
      body: body_slurs_removed,
      creator_id: creator.id,
      community_id: community.id,
      removed: None,
      locked: page.comments_enabled.map(|e| !e),
      published: page.published.map(|u| u.naive_local()),
      updated: page.updated.map(|u| u.naive_local()),
      deleted: None,
      nsfw: page.sensitive,
      stickied: page.stickied,
      embed_title,
      embed_description,
      embed_html,
      thumbnail_url: pictrs_thumbnail.map(|u| u.into()),
      ap_id,
      local: Some(false),
    };
    let post = blocking(context.pool(), move |conn| Post::upsert(conn, &form)).await??;
    Ok(post.into())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::objects::{
    community::ApubCommunity,
    tests::{file_to_json_object, init_context},
  };
  use assert_json_diff::assert_json_include;
  use serial_test::serial;

  #[actix_rt::test]
  #[serial]
  async fn test_parse_lemmy_post() {
    let context = init_context();
    let url = Url::parse("https://enterprise.lemmy.ml/post/55143").unwrap();
    let community_json = file_to_json_object("assets/lemmy-community.json");
    let community = ApubCommunity::from_apub(&community_json, &context, &url, &mut 0)
      .await
      .unwrap();
    let person_json = file_to_json_object("assets/lemmy-person.json");
    let person = ApubPerson::from_apub(&person_json, &context, &url, &mut 0)
      .await
      .unwrap();
    let json = file_to_json_object("assets/lemmy-post.json");
    let mut request_counter = 0;
    let post = ApubPost::from_apub(&json, &context, &url, &mut request_counter)
      .await
      .unwrap();

    assert_eq!(post.ap_id.clone().into_inner(), url);
    assert_eq!(post.name, "Post title");
    assert!(post.body.is_some());
    assert_eq!(post.body.as_ref().unwrap().len(), 45);
    assert!(!post.locked);
    assert!(post.stickied);
    assert_eq!(request_counter, 0);

    let to_apub = post.to_apub(&context).await.unwrap();
    assert_json_include!(actual: json, expected: to_apub);

    Post::delete(&*context.pool().get().unwrap(), post.id).unwrap();
    Person::delete(&*context.pool().get().unwrap(), person.id).unwrap();
    Community::delete(&*context.pool().get().unwrap(), community.id).unwrap();
  }
}
