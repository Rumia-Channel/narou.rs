ALTER TABLE novels ADD COLUMN attribute_tag TEXT;
CREATE INDEX novels_attribute_tag_idx ON novels(attribute_tag);
