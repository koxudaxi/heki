import acme.core.model as model
from ..settings import DEBUG
from .. import settings
import email.mime.text


def run():
    return model, DEBUG, settings
